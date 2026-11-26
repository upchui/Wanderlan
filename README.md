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
  to them too. Or, VLAN by VLAN, through the second site's own internet
  (local breakout), without them noticing.
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
  encrypts the traffic; wanderlan's tunnels (VXLAN) are not encrypted. UDP
  and TCP port 51821 and UDP port 51822 must pass through it.

Under the hood each site runs a small daemon (`wanderland`) with a
VLAN-aware Linux bridge and VXLAN tunnels to the other sites, and a terminal
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
  * **Servers** must reach each other and be reachable by their clients:
    UDP and TCP on the control port (default 51821: probes, configuration
    sync, joining) and UDP on the tunnel port (default 51822: the tunnels,
    VXLAN).
  * **Clients** only need to reach their server, outgoing: TCP and UDP to
    its control port, UDP to its tunnel port. A client may sit behind NAT,
    several behind the same one (see [Clients behind NAT](#clients-behind-nat)).
* The kernel modules `bridge`, `vxlan`, `veth`, `sch_ingress`, `cls_flower`,
  `act_vlan`, `act_gact` and `nf_tables` (standard kernels have them; the
  service loads them when it starts); for a local breakout also the NAT
  modules of nftables, which the kernel loads by itself.

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
addresses (only a [local breakout](#local-breakout) routes, and only the
public destinations of its VLANs).

### Servers and clients

The first site of a network is a **server**. A site that joins with an
invitation becomes a **client** of the site that invited it, like a cable
plugged into a switch: its only tunnel leads to that site (its *server*), so
a client only has to reach its server in the transport network. Servers are
connected with each other (every server with every other server), so servers
must reach each other.

A client can invite sites too; they become its own clients. It is then
**client + server**: a client in its tunnel to its server, the server in the
tunnels to its clients. For them it works like any server: it invites them,
takes over their changes and passes them on, and their clients can have
clients again. A site can be made a server or the client of another site
later. Every view shows the role (`server`, `client of hq`, `client of hq +
server`).

A site below a client + server stays below it: only its server tells a site
where it went, and moved elsewhere that client would no longer know it (a
server knows the whole network, so moving sites away from a server is fine).
To move one out, make the client + server a server first, or let the site
join again through its new server (`wanderlan reset` there, then `wanderlan
join`).

### Clients behind NAT

A client opens every connection to its server itself, and its server never
opens one towards it: the server only answers, and sends to where the
client's packets come from. So a client works behind NAT, at home behind a
router or on a mobile connection with a changing public address; several
clients behind the same NAT share its address and are told apart by their
identity.

Every tunnel is **VXLAN** on the tunnel port (UDP 51822), one interface per
neighbour. The tunnel between a client and its server runs in one of two
modes; there is nothing to set, the two daemons find out which one fits:

* **`nat`** (where every client tunnel starts): the client may sit behind
  NAT. It sends from the tunnel port, and every 15 s a small keepalive frame
  through its tunnel; that keeps the NAT's mapping open and shows the server
  which address and port the NAT uses for the client (nftables table
  `ip wanderlan`). When the NAT changes the port (it forgot the mapping, the
  router restarted), the server follows with the next keepalive, in place.
* **`direct`**: the server reaches the client at its address (the same LAN,
  a public address, a VPN). It sends there directly, both ends from a source
  port per connection, which spreads the load over the receive queues and
  cores of the network cards; no keepalives, nothing to learn.

**How it finds out:** now and then (at the start, then every 5 minutes, every
minute while it runs `direct`) the client asks its server for a test in one
of its authenticated probes and listens at its tunnel interface for two
seconds. The server sends a small test frame to the client's address at the
tunnel port, from another port than the tunnel port: a packet that no NAT
mapping and no firewall connection lets in unless the client is reachable
from outside. It carries a tag of the tunnel's keys over the request, so
nobody else can send it. If it arrives, both ends switch the tunnel to
`direct`; after two failed tests in a row they go back to `nat`, and the
pause before the next test doubles (up to a day), so a NAT that only stays
open while the client sends from the tunnel port cannot make it flap. Behind
NAT the test frame simply ends at the NAT. An older server or client never
tests: its tunnels stay `nat`. The mode and why show in `wanderlan status`
(`Tunnel to … : VXLAN direct: reached at …`), in the peer details of the
terminal UI and in the log.

Between servers the tunnels always run `direct`. Where connection tracking
is loaded (a firewall), a server with clients exempts the tunnel packets
from it (`notrack`, table `ip wanderlan`): tracking them would cost every
packet. A stateful firewall on such a server must therefore accept UDP to
the tunnel port by its port, not only as an established connection.

A site with clients of its own must be reachable for them, so it cannot sit
behind NAT itself. A carrier-grade NAT that gives the control traffic and the
tunnel packets of one box different public addresses is not supported (the
status says so). **Without a VPN the traffic crosses the internet
unencrypted:** the keys of the tunnels authenticate, they do not encrypt.

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

### Local breakout

Normally the devices of a site reach the internet like everything else:
through the tunnel and the gateway (and firewall) of the network. A **local
breakout** lets one VLAN of one site use that site's own internet instead,
completely transparently: its devices keep their gateway, DHCP and DNS, but
every frame they send to a **public IPv4 or IPv6 address** leaves through
the uplink of the box at their site, masqueraded to its address, and the
answers come back to them. Everything else keeps going through the tunnel:

* private destinations (10/8, 172.16/12, 192.168/16, 100.64/10, link-local,
  ULA `fc00::/7`, multicast, the NAT64 prefixes), so the NAS, the printers
  and the other VLANs are reached as before;
* the **exceptions** of the VLAN, e.g. a service that only lets in the
  public address of the central site (`except = ["203.0.113.0/24"]`).

IPv6 breaks out only what goes **to a router of the VLAN**: the box learns
its MAC address from its router advertisements and its neighbour
advertisements (router flag). So devices of the VLAN with global addresses
still reach each other directly, and until a router was seen IPv6 stays in
the tunnel. The router's own public address breaks out too; add it to the
exceptions if it must be reached through the tunnel.

The **uplink** is the interface of the box's default route, or one chosen
per VLAN (`uplink = "ens20"`, e.g. a second line; it needs a default route
of its own, with any metric, or a point-to-point link such as WireGuard).
Only frames from the trunk of the site break out, never what comes from a
tunnel, and nothing of a breakout ever reaches a tunnel: the central site
does not see it at all. If a family cannot break out (no IPv6 uplink, no
default route on the chosen interface, forwarding cannot be switched on), it
goes through the tunnel as before, and the status says why.

A breakout bypasses the firewall of the network, so **its manager decides**,
like for the VLANs of a site (Breakout (`7`), or `wanderlan breakout`).


### Who knows what

Servers know the whole network. A client knows only itself, its server and
the sites below it (its clients, their clients, ...): it never learns about
the clients of other sites, neither in the terminal UI nor in its
configuration files. Changes made on a client (to itself and to the sites
below it) reach the network through its server; a client + server passes the
changes of its clients on the same way, so they travel up through every site
on the way, and each checks them.

### Who may change what

The site that created an invitation **manages** the site that joined with it.

| Change | Who |
|--------|-----|
| VLANs, mappings, local breakout, online state of a managed site, removing it | its manager; the site itself too if it was released ("may change its own VLANs") |
| The same for a site without a manager (the first site) and the VLANs of the network | every site that is not *restricted* |
| Timeouts, MTU check, MSS, control port, invitations | every site |
| The name of a site | the site itself, always; its manager only if the site allows it |

A site that joined and was not released is **restricted**: it cannot change
the VLANs its server gave it, only those of sites it invited itself. Its own
VLANs (see above) are always its own business. A client may in any case only
change itself and the sites below it, never its server. The daemons enforce
these rules among themselves (someone with root on a site could bypass
them); which box is behind a site, however, is protected by its key (see
[Identities and keys](#identities-and-keys)).

**Renaming a site.** Sites (`2`), `N` (only the new name), or `wanderlan
rename <site> <new-name>`. A site may always rename itself, also a
restricted one; its manager (`managed_by`, "Changes") may rename it too; a
client never renames its server. Its identity stays, so every site
recognises the renamed one: the network, `self` in its own
`local.toml` and the trunks per tunnel of the others follow, and the
tunnels to it are built anew (about a second without traffic on them).
Older sites do not understand a rename made by their client; update them
first.

### Identities and keys

Every box has an **identity**: a key pair it creates the first time it is
configured or joins (`/var/lib/wanderlan/identity`, readable by root only).
The public half is the site's `id` in `network.toml`, like a WireGuard
public key; `wanderlan status` shows it, the terminal UI its first
characters. The two sites of a tunnel derive the keys of that tunnel from
their identities; no key travels over the network or is written to a file:

* the **VNI** of the tunnel (24 bits): the kernel drops tunnel packets
  with another one, so a second box at the same address (behind the same
  NAT) or a cloned machine does not get into the network;
* the **probe key**: every control probe carries a tag, and a tunnel comes
  up only once the other site has answered with its key ("identity
  confirmed"). A box at its address that answers with another key keeps the
  tunnel down ("wrong key");
* the **sync key**: a site takes a network only from the neighbour that
  holds the key, and gives its own only to it.

Joins tell boxes apart the same way. The same box joins again (also from a
new address). A box that was reset (`wanderlan reset` deletes the identity:
it is a new box afterwards) takes over its old entry, with all its rules,
only if the old box does not answer any more; a second box at the address
of a site that is still running is refused. A box with the identity of a
site that runs elsewhere (a cloned machine) is refused too: run `wanderlan
reset` on it first. Identities are never edited by hand: a `network.toml`
written without the `id` lines keeps them.

The keys **authenticate, they do not encrypt**: the traffic between the
sites is only as private as the transport network (the VPN). The VNI is
visible in every packet; it tells boxes apart but does not stop someone who
can see the traffic.

Until a site knows the identity of a neighbour (right after an update, for
a moment), their tunnel takes a VNI from the two names and comes up without
proof of identity; probes and sync are not authenticated then. Sites of
older versions run GRE and get no tunnel until they are updated (update all
sites).

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

**Take a site online or offline.** `o` on the site in Sites or in the
list of sites of the Overview (the Overview goes straight to the review);
or `wanderlan online <site> --confirm` / `wanderlan offline <site> --confirm`.
A site that joined with an invitation starts offline; the Overview names
the sites that are offline.

**Add or remove a site by hand.** Sites, `n` (a client of this site unless
"Server" says otherwise) or `d` (not while it has clients). CLI:
`wanderlan site add lab 10.8.0.5 --server hq --vlans 10,20`,
`wanderlan site remove lab`.

**Choose the VLANs of a site.** Sites, `e` on the site, field "VLANs"
(empty: the VLANs of the network). The VLANs of the network: VLANs (`3`),
`v`, or Sites (`2`), `m`. CLI: `wanderlan site vlans lab 10,20` (`default`:
those of the network), `wanderlan vlans native,10-50` (`all`: every VLAN).

**Give a site VLANs of its own.** VLANs (`3`), `o`: VLANs on this site's
trunk and towards its own clients, never to its server; possible even when
the server manages everything else. CLI: `wanderlan own-vlans <site> 50,60`
(`none` removes them).

**Put untagged traffic into a VLAN, translate a VLAN.** VLANs (`3`), `n`:
site, source VLAN (e.g. `30`), mapped VLAN `native` (or the VLAN ID on that
site's trunk, e.g. `130`). CLI: `wanderlan map add lab 30 native`,
`wanderlan map add lab 40 140`, `wanderlan map del lab 30`.

**Let a VLAN use the internet of its own site.** Breakout (`7`), `n`: site,
VLAN, uplink (empty: the default route; the window names the interfaces of
this box) and exceptions; `Enter` changes one, `d` removes it. The view
shows for this site where IPv4 and IPv6 leave now and the traffic. CLI:
`wanderlan breakout office 10,20 on`, `wanderlan breakout office 20 on
--uplink ens20 --except 203.0.113.0/24`, `wanderlan breakout office 20 off`;
`wanderlan breakout` shows the breakouts of this site.

**Let a site change its own VLANs, move it.** Sites, `e` on the site:
"Changes" (nobody, so every server may change it; a site as its manager;
or the manager and the site itself) and "Server" (none: a server; or the site it is a client of). A
site added by hand is a client of this site. CLI:
`wanderlan manage <site> --self-managed yes`,
`wanderlan role <site> client <server>`, `wanderlan role <site> server`.

**Limit what one tunnel carries.** VLANs (`3`), `t` lists every tunnel with
what it carries; `Enter` limits it (e.g. `30-40`, or `none`), `d` lifts the
limit. CLI: `wanderlan tunnel-vlans dc lab 30-40` (`all` lifts it). Whoever may
change one of the two sites may limit their tunnel.

**Use several trunk ports.** The trunk can be several local interfaces;
they are bridged like the ports of a switch and all carry the same VLANs and
mappings (the first is the main port). Settings (`8`), "Trunk", lists the
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
it, and a client which ports are its trunk. Settings (`8`), "Trunk per
tunnel" (the same list of interfaces; nothing chosen is the default trunk),
or `wanderlan trunk <site> ens20` / `ens20,ens21` (`default` to undo).

```
hq
  ens19 ─┐
  ens22 ─┴ br0 ───────┬── vx-office ══ office
                      └── vx-lab    ══ lab
  ens20 ── wl-ens20 ───── vx-dc     ══ dc
```

**Traffic graphs.** The daemon records the traffic of its tunnels and trunk
ports (every 2 s) and every round trip of the last hour, so the graphs are
full as soon as `wanderlan` starts. What came in is drawn above the line
(green), what went out below it (magenta), each with a round scale of its
own; the title shows now, the mean and the peak. `z` switches between the
last 2 minutes, 10 minutes and the hour. The Overview shows all tunnels
together (on a tall enough terminal) and a LOAD column per neighbour on one
scale; the details of a neighbour its traffic and, below it, its round trips
with the lost probes in red.

**Watch the traffic of one VLAN.** Overview (`1`), `Enter` on a neighbour:
beside its traffic graph the VLANs of the tunnel with what comes in and goes
out now, those that carried traffic first. `Up`/`Down` chooses one, `Enter` opens it: every
port of this site with the VLAN (tunnels and trunk ports), in and out now,
bytes and packets, the MAC addresses behind each port, a graph, where the
VLAN is on the trunk (mapped or not), which tunnels carry it and where this
site passes it on. `m` shows its MAC table. CLI: `wanderlan vlan 20`.

**Start over on a site.** Setup (`6`), `R`, or `sudo wanderlan reset`: the
bridges, tunnels, all rules, the configuration and the invitations are
removed and the site is unconfigured again, like after the installation.
Join again with a new
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
  its part of it (itself, its server, the sites below it). The daemons keep the
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

`network.toml` on `hq` (a server). Every site is a table `[site.<name>]`
with everything about it: its server, its VLANs, its mappings and the limits
of its tunnels.

```toml
[network]
# vlans = "all"               # VLANs of every site without its own list
commit_timeout = "60s"        # changes are rolled back unless confirmed in time
# mtu_probe_interval = "5m"   # how often the path MTU to each peer is measured
# mss = 1300                  # fixed TCP MSS instead of measuring
# control_port = 51821        # UDP/TCP port for probes, sync and joining
revision = 12                 # maintained by wanderlan

[site.hq]
address = "10.8.0.1"          # address in the transport network

[site.dc]                     # a second server (no `server`)
address = "10.8.0.2"

[site.office]
address = "10.8.0.11"
server = "hq"                 # a client of hq
managed_by = "hq"             # invited by hq
vlans = "10,20"               # office only takes part in VLANs 10 and 20
map = { 20 = 120 }            # source VLAN 20 = VLAN 120 at office

[site.lab]
address = "10.8.0.12"
server = "hq"
managed_by = "hq"
online = false                # joined, not taken online yet
map = { 30 = "native" }       # source VLAN 30 = untagged at lab
tunnel_vlans = { hq = "30-40" }  # the tunnel to hq carries at most 30-40
# VLAN 30 reaches the internet through lab's own uplink, except 203.0.113.0/24
breakout = { 30 = { except = ["203.0.113.0/24"] } }
```

`map` reads like the VLAN mapping view: `source VLAN = VLAN on the trunk of
this site` (or `"native"` for the untagged traffic). The `network.toml` of
`lab` contains only `hq` (as its server) and `lab`.

Files of wanderlan 1.0.20 and older (`[[site]]` with `name`, `[[map]]`,
`[[tunnel]]`) are still read. The daemon writes such a file in the new form
when it starts and keeps the old one as `network.toml.v1` (an older version
can only read that one). The sites exchange the network in the old form, so
sites with older and newer versions keep working together.

### Settings

Every site sets its own settings, each on itself (`wanderlan settings`, or
Settings `8` in the terminal UI): the commit timeout, the path MTU check
and a fixed MSS in its `local.toml`, its ports in its own entry of the
network (the other sites connect there; only the site itself may change
them). The values of `[network]` are only the defaults of the sites
without their own; they cannot be changed any more (they would change the
other sites).

| Key | Default | Meaning |
|-----|---------|---------|
| `network.vlans` | all | VLANs of every site without its own list. Missing, empty or `"all"`: every VLAN and the untagged traffic. Otherwise e.g. `"native,10-50"`, `"10,20,30-40"` or `[10, "20-30"]`. |
| `network.commit_timeout` | `60s` | Default time to confirm a change (10 s to 1 h). |
| `network.mtu_probe_interval` | `5m` | Default interval of the path MTU measurement (at least 10 s). |
| `network.mss` | unset | Default fixed TCP MSS (536 to 1460) instead of the measured value. |
| `network.control_port` | `51821` | Default UDP and TCP port of the daemons. |
| `network.tunnel_port` | control port + 1 (`51822`) | Default UDP port of the tunnels (VXLAN). |
| `network.revision` | `0` | Revision counter of the sync; raised by the daemon. |
| `[site.<name>]` | | One table per site. The name is unique; `a-z`, `0-9` and `-`, at most 12 characters (it is part of the interface name `vx-<name>`). |
| `site.<name>.address` | | Unique IPv4 address of the site in the transport network. |
| `site.<name>.server` | none | The site this one is a client of; none: a server. |
| `site.<name>.managed_by` | none | The site that manages this one (the inviting site). |
| `site.<name>.self_managed` | `false` | The site may also change its own VLANs, mappings and online state. |
| `site.<name>.online` | `true` | `false`: the tunnels of the site stay down. Sites that join start with `false`. |
| `site.<name>.vlans` | `network.vlans` | VLANs this site takes part in. |
| `site.<name>.own_vlans` | none | The site's own VLANs: on its trunk and towards its own clients, never towards its server. The site itself may always change them. |
| `site.<name>.map` | none | VLAN mappings: `{ <source VLAN> = <mapped VLAN> }`. The source VLAN is the VLAN in the network (one of the VLANs of the site, not 1), the mapped VLAN its ID on the site's trunk or `"native"` (untagged), e.g. `{ 30 = "native", 40 = 140 }`. |
| `site.<name>.tunnel_vlans` | none | The most the tunnel to another site carries (on top of the VLANs of both sites), e.g. `{ dc = "30-40" }`; `"none"`: no VLAN at all. At one of the two sites. |
| `site.<name>.control_port` | `network.control_port` | UDP and TCP port of this site's daemon. Only the site itself changes it (`wanderlan settings control-port`). |
| `site.<name>.tunnel_port` | its control port + 1 | UDP port of this site's tunnels; a tunnel through NAT also sends from the one above. Only the site itself changes it. |
| `site.<name>.breakout` | none | [Local breakout](#local-breakout) per VLAN of the site (its ID in the network; untagged: 1): `{ <VLAN> = { uplink = "<interface>", except = [<networks>] } }`, both optional, e.g. `{ 10 = {}, 20 = { uplink = "ens20", except = ["203.0.113.0/24"] } }`. Older sites do not read a network with it: update all sites first. |
| `self` (local) | | Which site of `network.toml` this host is. |
| `trunk` (local) | | The interface carrying the VLANs, or several (`["ens19", "ens22"]`, bridged like switch ports, the first is the main port). |
| `tunnels.<site>.trunk` (local) | `trunk` | A trunk of its own for the tunnel to `<site>`: one interface or several. A port belongs to one segment. |
| `commit_timeout` (local) | `network.commit_timeout` | Time to confirm a change on this site (10 s to 1 h). |
| `mtu_probe_interval` (local) | `network.mtu_probe_interval` | How often this site measures the path MTU (at least 10 s). |
| `mss` (local) | `network.mss` | Fixed TCP MSS (536 to 1460) on this site's tunnels instead of the measured value. |

Every file is checked when it is loaded or applied; problems are reported
with file and line:

```
network.toml:24: error: site "office": vlans: VLAN ID 5000 is outside 1-4094
network.toml:26: error: map #2: source VLAN 60 is not one of the VLANs of site "office" (10,20)
```

Names and addresses are unique, servers are existing sites and never form
a circle, mappings use VLANs of their site (at most one native mapping
per site, no mapped VLAN used twice, no source VLAN mapped twice per site),
`self` exists, and unknown keys are errors (typos). Older files load
too: `role` and `hub` (wanderlan 0.1) are ignored with a warning, and in the
old form (`[[site]]`) a site with `managed_by` but without `uplink` is a
client of its manager.

## How it works

### On each site

The daemon creates, and removes again whenever the service ends (a stop,
an update, a crash or a kill: systemd then runs `wanderland --cleanup`, and
once more before every start in case that could not run):

* `br0`: a bridge with VLAN filtering, STP off, no default VLAN and no
  netfilter calls of its own (if `br_netfilter` is loaded, e.g. by Docker,
  with `net.bridge.bridge-nf-call-iptables = 1`, untagged frames still pass
  iptables; the daemon warns about it), counting the traffic per VLAN and port
  (`vlan_stats_per_port`), with the trunk as a port. The kernel takes the
  counters only from a new bridge, so a bridge of an older version is built
  anew once (a short interruption); on a kernel without them the bridge
  stays as it is and the traffic per VLAN is not shown;
* `wl-<interface>`: the same for every further trunk ("trunk per tunnel");
* `vx-<peer>`: a VXLAN tunnel (UDP 51822, the VNI of the two identities,
  no learning, no UDP checksum and no DF outside, MTU 1500) to its server,
  to each of its clients and, on a server, to every other server. It sends
  from any address of the box; where to is its default FDB entry
  (`bridge fdb show dev vx-<peer>`: `00:00:00:00:00:00 dst …`), for a client
  behind NAT the address and port its packets come from. In `nat` mode
  both ends send from the tunnel port, otherwise from a port per
  connection. The ports between servers are `isolated` (split horizon: a
  frame from one server is never forwarded to another server); the ports of
  clients and uplinks are ordinary ports, a tree that is loop-free by itself;
* the VLANs of the site on the trunk (VLAN 1 untagged as the native VLAN) and
  on every tunnel the VLANs both ends take part in;
* tc `clsact` filters on the trunk for translations (recognised by their
  action cookie `wl:…`), all in one flower priority per direction
  (`0xc000`, handle = VLAN ID), so a frame costs one lookup however many
  translations there are;
* the nftables table `bridge wanderlan` for MSS clamping: every bridged
  frame of the host passes one rule there, only TCP SYNs go on to the MSS
  rules (chain `wl-mss`);
* with a [local breakout](#local-breakout), per VLAN and segment a veth
  pair: `wlbp<vid>` is a port of the bridge (only in that VLAN, untagged,
  with a static forwarding entry), `wlbo<vid>` its other end in the IP stack
  of the box (MAC `02:77:6c:…`, address `192.0.0.8/32`, the IPv4 dummy
  address of RFC 7600, as the sender of its ARP requests, so no real
  address gets into the caches of the devices). The table `bridge
  wanderlan-breakout` rewrites the destination MAC of the frames from the
  trunk with a public destination to that of `wlbo<vid>`, so the bridge
  delivers them there instead of to the tunnel, and keeps everything of the
  breakout away from the tunnels; the table `inet wanderlan-breakout` marks
  their connections, clamps the MSS and masquerades them. The answers find
  `wlbo<vid>` by their mark (routing rules and tables of protocol 87, so the
  box never needs to know the addresses of the VLAN; a chosen uplink gets a
  table with its default route). Forwarding is switched on where it is
  needed (IPv4 on `wlbo*` and the uplinks, IPv6 for the whole box: the
  kernel has no switch per interface; `accept_ra = 2` on the uplinks, so
  they keep their router advertisements); what was changed is written down
  first (`/var/lib/wanderlan/breakout-sysctl`) and put back when the
  breakout ends or the service stops. The unit may write
  `/proc/sys/net/ipv{4,6}/conf` for that, the rest of `/proc/sys` stays
  read-only. All of it runs in the kernel; only the breakout traffic is
  connection-tracked;
* with client tunnels: the nftables table `ip wanderlan`. In `nat` mode both ends
  send from the tunnel port: the VXLAN interface sends from it or the port
  above (Linux includes the upper end of a port range since March 2026), and
  a rule in output (chain `out`) puts the tunnel port back into the packets
  of these tunnels (by their VNI). On the server a tunnel packet to the
  tunnel port costs one compare of its inner ethertype (data frames always
  carry a VLAN tag); only the keepalives of the clients go on and put their
  source address and port into the set of their client (`ep-<client>`,
  recognised by the VNI).

Everything it creates carries the interface alias `wanderlan`; foreign
interfaces are never touched (a foreign `br0` or `vx-*` stops the
configuration with an error). After a restart it adopts what is there
without changes. After a topology change it flushes the learned MAC
addresses, again after 10 s and 30 s while the other sites follow.

### Synchronisation

The daemons talk to their tunnel neighbours over UDP on the control port
every 5 s: reachability, round trip, path MTU and the revision and
fingerprint of their configuration. Every probe carries a tag of the probe
key and every answer one over the nonce of its request, so an answer proves
that the neighbour holds its key now; between sites that know each other's
identity the configuration is fetched with an authenticated exchange
(`SYNC`), otherwise (a site of an older version, or one that does not know
the other's identity yet) as before. Each neighbour is told the version it
should have: servers and the uplink the whole configuration of this site, a
client only its own part. A site that sees a newer version fetches it over
TCP and applies it; a server that sees changes of one of its clients fetches
the client's part and takes over what the client is responsible for (itself,
the sites below it, the network settings), checked against the rights of the
client and of the sites below it (their changes come through it). Changes
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

`MSS = path MTU − 20 (outer IP) − 8 (UDP) − 8 (VXLAN) − 18 (Ethernet + VLAN tag) − 40 (inner IP + TCP)`

(IPv6: 20 bytes less; ports without a measurement use the smallest measured
path). Larger packets still arrive, also with DF set: the tunnel packets are
fragmented on the way (no DF outside) and put together at the other end.

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
logs it and shows it in the TUI until `wanderlan unblock vx-<site>` (`u` on
the peer in the TUI; a blocked trunk port: `U`) or the configuration changes.

### Operation

`wanderland` runs as the systemd service `wanderland`, as root with
`CAP_NET_ADMIN` and `CAP_NET_RAW` (for the keepalive frames of the client
tunnels) as its only capabilities and a hardened unit. systemd restarts
it after a crash: the cleanup removes what the crashed daemon left, the new
one builds the network again within seconds. An update restarts it the
same way. Only one daemon
runs at a time: `wanderland` started from a shell only explains that it is
the daemon. Logs go to the journal (`journalctl -u wanderland`) and to the
event log (`wanderlan log`).

## Terminal UI

`wanderlan` without arguments starts the TUI: keyboard first, works over SSH
and needs at least 80×24 (larger terminals show more). `?` shows all keys,
starting at the section of the current view; the bar at the bottom shows the
most important ones of each view. Columns are as wide as their content; a
text cut short ends with `…` and the line below the list shows the selected
row in full. Every list
scrolls with Up/Down, PgUp/PgDn, Home and End and shows a scrollbar when it
holds more than fits; `/` searches in Sites, VLANs, MACs and Log. `--mouse`
enables the mouse: a click selects a row (a second click opens it), the
wheel scrolls the list under the pointer, the keys in the bottom bar can be
clicked.

| Key | View |
|-----|------|
| `1` | Overview: this site with its role, the tunnel neighbours live (reply, round trip, path MTU, MSS, port state, MAC addresses, throughput, config in sync or `refused`), traffic of the tunnels and of every trunk, a graph of all tunnels and the LOAD of each over time (`z`: 2 min, 10 min, 1 h), and all known sites with role and link (`via hq` for sites without a tunnel from here), each server with its clients indented below. One cursor runs through the neighbours and the sites. `Enter` on a neighbour: traffic graph, the VLANs of the tunnel with their live traffic (Up/Down, `Enter` opens one: every port, MACs, graph; `m` its MAC table, PgUp/PgDn scroll its ports), round-trip graph with the lost probes, min/avg/max round trip, jitter, probe loss, counters, where it is probed (its address and ports) and, without answers, what to check (Left/Right: the next neighbour); on a site: its details. `o` on a site takes it online or offline (to the review), `u` / `U` unblock the port of a neighbour / a trunk port, `p` measures the path MTU. |
| `2` | Sites: name, address, VLANs, role, manager, tunnel; clients indented below their server. `Enter`: details (role, address and ports, VLANs, every tunnel with what it carries, mappings, who may change it; Up/Down: the next site). Invite (`i`), open invitations (`I`), online/offline (`o`), new (`n`), edit (`e`: name, address, VLANs, "Server", "Changes"), rename (`N`), delete (`d`), the VLANs of the network (`m`), trunk ports of this host (`l`). |
| `3` | VLANs: the VLANs of the network (`v`), the own VLANs of this site (`o`) and the mappings (`n`, `Enter`, `d`) with their site and its role; conflicts are marked right away. `t` switches to the tunnels: what each carries, its limit (`Enter`, `d`). |
| `4` | MAC addresses: filter (`/`; `vlan:20` for one VLAN), sort (`s`, `o`). |
| `5` | Log: live events in local time (`t`: UTC), filter by level (`l`), site (`p`) and text (`/`). |
| `6` | Setup: new network or join; on a configured site invite (`i`) and reset (`R`). |
| `7` | Breakout: the [local breakouts](#local-breakout) of all sites, per VLAN, with uplink and exceptions; for this site where IPv4 and IPv6 leave now (or why they go through the tunnel) and the traffic, above it the uplinks of this box. New (`n`), edit (`Enter`), remove (`d`). |
| `8` | Settings of this host only (this site, trunk, commit timeout, path MTU check, MSS, its ports), each with an explanation, the trunk of every tunnel, and the role and rights of this site. `Enter` on a trunk lists the interfaces of the host to choose from (`Space`, `m`: main port). |

Changes are collected in a draft; the status line says how many it holds,
and new or changed rows are marked `+` or `*`. `a` shows the difference to
the running configuration and applies it; a banner counts down until it is
confirmed (`c`) or rolled back (`r`, asks first), also while a window is
open. `x` discards the draft, `q` quits; where a wrong "yes" would lose
something (reset, discard, roll back, quitting with a draft, closing a form
with input) Enter means no and only `y` is yes. Messages that do not fit get
a second line, `M` shows the last ones, and the status line of every view
names problems of this site (an error, blocked ports, refused changes,
tunnels that are down). When the daemon is not reachable (`no daemon` in the
status line), the TUI shows the state of the service and its last log lines
and keeps trying.

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
| `wanderlan manage <site> [--by S] [--self-managed yes\|no]` | Who may change (and rename) a site |
| `wanderlan rename <site> <new-name>` | Rename a site: this site always, a site it manages too |
| `wanderlan role <site> server\|client <server>` | Make a site a server or a client |
| `wanderlan site add <name> <address> [--server S\|none] [--vlans V]` | Add a site by hand (default: a client of this site, managed by it) |
| `wanderlan site remove <name>` | Remove a site with its mappings (not while it has clients) |
| `wanderlan site vlans <name> <vlans\|default>` | The VLANs of a site |
| `wanderlan vlans <vlans\|all>` | The VLANs of the network |
| `wanderlan map add <site> <source> <mapped\|native>` / `wanderlan map del <site> <source>` | VLAN mappings: source VLAN = mapped VLAN on the site's trunk |
| `wanderlan settings [<setting> <value\|default>]` | The settings of this site (`commit-timeout`, `mtu-check`, `mss`, `control-port`, `tunnel-port`): shown, or one changed. Only this site's: every site sets its own |
| `wanderlan trunk <site> <interfaces\|default>` | Trunk ports of the tunnel to a site, or (this site's name) of the default trunk; several separated by commas |
| `wanderlan own-vlans <site> <vlans\|none>` | Own VLANs of a site (trunk and its own clients only) |
| `wanderlan tunnel-vlans <site> <site> <vlans\|all\|none>` | The most the tunnel between two sites carries |
| `wanderlan breakout [<site> <vlans> on\|off [--uplink IF\|auto] [--except NETS\|none]]` | Local breakout of VLANs of a site; without arguments the breakouts of this site and its uplinks |
| `wanderlan apply [--network F] [--local F] [--timeout 60s]` | Apply files (default: those in `/etc/wanderlan`) |
| `wanderlan confirm` / `wanderlan rollback` | Confirm or revert the pending change |
| `wanderlan validate <network.toml> [--local <local.toml>]` | Check files without applying them |
| `wanderlan init [--dir D]` / `wanderlan export` | Print or write the example configuration / print the running `network.toml` |
| `wanderlan probe` / `wanderlan unblock <port>` | Measure the path MTU now / re-enable a port the loop guard shut down |
| `wanderlan reset [--yes]` | Remove everything wanderlan set up on this site |

Commands that change the configuration (`online`, `offline`, `manage`,
`role`, `site`, `vlans`, `map`, `trunk`, `own-vlans`, `tunnel-vlans`,
`breakout`, `settings`) take `--confirm` to confirm right away; without it, run
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
  server's), and the switch port or VM must carry it tagged. UDP to the
  tunnel port (51822) must pass between the site addresses:
  `tcpdump -ni any udp port 51822`.
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
* **"waiting for client … to connect"** on a server: no keepalive of the
  client arrived yet; UDP to the tunnel port (51822) must pass from the
  client to the server (a firewall in front of the server?). The client
  sends one every 15 s once its own tunnel is up.
* **"identity not confirmed yet"**: the tunnel waits for an answer to the
  control probes with the neighbour's key; the probes do not get through
  (see above), or the neighbour runs an older version (update it).
* **"answers with another key"** / **"wrong key"**: another box answers at
  the neighbour's address, e.g. one that was reset or a second box behind
  the same NAT. A reset box joins again with an invitation; every box needs
  its own address in the transport network.
* **"another box joined as …"** on a site: its entry now belongs to another
  box (a reset copy, a cloned machine). Reset this box and join again, or
  remove the other one.
* **A breakout VLAN does not reach the internet**: `wanderlan breakout` says
  where IPv4 and IPv6 leave, or why they go through the tunnel. A firewall
  on the box that drops forwarded traffic (ufw's default, Docker's `FORWARD`
  policy) must let the traffic from `wlbo*` out to the uplink; the status
  and the Breakout view (`7`) name such a chain. Where
  `/proc/sys` is read-only (a container), forwarding must be on already and
  the source checks loose (`net.ipv4.conf.all.rp_filter = 2`). A VLAN with
  public addresses of its own (a DMZ) needs its network as an exception.
  Two segments of one box with a breakout must not use the same addresses.
* **Large transfers stall**: compare the path MTU in `wanderlan status` with
  the MTU of the VPN; set `mss` if probes are filtered.
* **A port is BLOCKED**: the loop guard found a loop through another layer-2
  path between sites; remove it, then `wanderlan unblock vx-<site>`.
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
  VPN; wanderlan only does layer 2 and VLANs, and VXLAN runs unencrypted on
  top of that trusted network.
* **VXLAN for every tunnel.** L2 over UDP that every kernel segments and
  aggregates (GSO/GRO) and many network cards offload, through NAT as well.
  GRE inside UDP (FOU) was the first try: on kernels before October 2026
  (Debian 13 among them) GRO loses the UDP tunnel type of aggregated GRE
  packets and the kernel drops them, which cut a 10 Gbit/s LAN down to
  13 Mbit/s.
* **The client opens every flow (`nat`).** A server never contacts a client
  in `nat` mode first, so clients work behind NAT; the server learns the
  NAT's port from the client's keepalives in the kernel (nftables), so
  nothing per packet runs in user space. A client that its server reaches
  directly runs `direct`, like the servers among each other; the daemons
  find that out by themselves with a test frame (no setting).
* **A site is a box with a key, not an address.** Every box has an X25519
  identity; the keys of a tunnel are derived from the two identities
  (Diffie-Hellman, HKDF-SHA256), so they never travel and no third site knows
  them. They authenticate the probes, the sync and (as VNI) the tunnel
  packets; they cost nothing per packet.
* **Servers and clients, like plugging switches together.** Roles are not
  configured up front: a site that joins is a client of the site that invited
  it and only has to reach it; servers are all connected with each other,
  with split horizon. There are no tunnels between clients; the VLANs decide
  who meets whom.
* **Clients only know their part.** A client's configuration contains its
  server and the sites below it only; servers merge the changes of their
  clients, a client + server passes those of its clients on.
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
  `IP_PMTUDISC_PROBE`): no raw sockets for them, and the same probes carry
  the sync announcements (`CAP_NET_RAW` is only for the keepalive frames of
  the client tunnels).
* **MSS clamping on entering and leaving SYNs** per tunnel port, so both
  directions get the smaller value. Only what goes through a tunnel is
  clamped: the local breakout keeps the MSS of the devices and lowers it only
  where its uplink itself is smaller (to the MTU of the route, e.g. PPPoE);
  no interface MTU is changed.
* **netlink everywhere except nftables**: links, bridges, VLANs, VXLAN with its FDB entry and tc
  use rtnetlink; nftables is driven through its JSON interface (the
  `nftables` crate, hence the dependency on the `nftables` package; a test
  guards a workaround for swapped `<`/`>` in its 0.6.3 release).
* **The history for the graphs lives in the daemon**: an hour of samples
  per tunnel and trunk port in memory (a few tens of KB each), fetched by
  the terminal UI once and then only what is new (`history` request).
* **Fixed interface names** `br0`, `wl-<interface>`, `vx-<site>` and
  `wlbo<vid>`/`wlbp<vid>`; site names have at most 12 characters so that
  `vx-<site>` fits.
* **Local breakout by destination, not by gateway.** The devices keep their
  gateway; the box takes the frames to public destinations out of the
  bridge before they reach the tunnel, and masquerades them (IPv6 too, so
  dual-stack devices do not keep using the tunnel). The answers return by a
  connection mark, so the box needs no address in the VLAN; a family that
  cannot break out stays on the tunnel. The manager of a site decides about
  it, like about its VLANs.

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
