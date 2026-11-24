# wanderlan

wanderlan connects several sites on layer 2, so that the same VLANs, subnets
and DHCP servers are available at every site as if it were one network.

wanderlan does **not** build a VPN. The sites must already reach each other
through an existing transport network, usually a VPN set up beforehand
(WireGuard, IPsec, a provider network, …). Every site has an **address** in
that network. On top of it wanderlan builds the layer-2 part:

```
Clients ─ trunk ─ br0 ─┬─ gt-<peer1> ─┐
                       └─ gt-<peer2> ─┴─ existing VPN / transport network
```

* one GRETAP tunnel `gt-<peer>` per peer site, between the site addresses,
* a Linux bridge `br0` with VLAN filtering (STP off) with the local trunk and
  all tunnels as ports,
* VLAN mappings per site (untagged → VLAN, VLAN → VLAN),
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

Sites have one of two roles:

| Role  | Tunnels |
|-------|---------|
| hub   | to all other hubs and to its own spokes |
| spoke | exactly one, to its hub |

Two hubs without spokes are the simple two-site case; only hubs give a full
mesh. Spokes are for sites that only reach a hub through the transport
network, e.g. behind CGNAT with a VPN to that hub.

## Installation

Add the APT repository and install the package:

```
curl -fsSL https://upchui.github.io/Wanderlan/key.gpg | sudo gpg --dearmor -o /usr/share/keyrings/wanderlan.gpg
echo "deb [signed-by=/usr/share/keyrings/wanderlan.gpg] https://upchui.github.io/Wanderlan/apt stable main" | sudo tee /etc/apt/sources.list.d/wanderlan.list
sudo apt update && sudo apt install wanderlan
```

On minimal installations `curl` and `gpg` may be missing:
`sudo apt install curl gpg`.

Updates come with `sudo apt upgrade`. Older versions stay in the
repository: `apt-cache madison wanderlan` lists them and
`sudo apt install wanderlan=<version>` downgrades.

The package installs

| Path | Content |
|------|---------|
| `/usr/bin/wanderland` | the daemon |
| `/usr/bin/wanderlan` | terminal UI and command line |
| `/usr/lib/systemd/system/wanderland.service` | systemd unit, enabled and started |
| `/etc/wanderlan/mesh.toml`, `local.toml` | configuration (conffiles, empty templates) |
| `/usr/share/doc/wanderlan/examples/` | example configuration |
| `/usr/lib/modules-load.d/wanderlan.conf` | kernel modules loaded at boot |

and creates the group `wanderlan`. Members of this group can use the TUI and
the command line without root:

```
sudo usermod -aG wanderlan $USER    # then log in again
```

## Quick start

On the **first site** (say a hub `heim` at transport address `10.8.0.1`):

1. Run `wanderlan`. On a site without configuration the setup wizard opens:
   site name, role, trunk interface (chosen from the host's interfaces),
   the site's transport address, the shared VLANs (e.g. `10-50`) and a first
   peer (name, role and address, e.g. hub `buero` at `10.8.0.2`).
2. Review the change and apply it; then confirm it with `c` within the
   commit timeout (60 s by default), otherwise it is rolled back.

On the **second site** run the wizard the same way (site `buero`, peer
`heim`). Once both sites list each other, the tunnel comes up.

To add further sites, add them in the TUI of any existing site (view 2,
`n`) and confirm. The configuration is distributed to all sites
automatically. On the new site the wizard only needs its own name, trunk,
address and the hub it connects to; the full mesh configuration then
arrives from that hub.

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
vlans = "10-50"               # VLANs stretched across all sites
mtu_probe_interval = "5m"     # how often the path MTU to each peer is measured
# mss = 1300                  # optional: fixed TCP MSS instead of measuring
commit_timeout = "60s"        # changes are rolled back unless confirmed in time
# control_port = 51821        # UDP/TCP port for probes and sync on the site addresses
# revision = 12               # maintained by wanderlan, do not edit by hand

[[site]]
name = "heim"
role = "hub"
address = "10.8.0.1"          # address of the site in the transport network

[[site]]
name = "buero"
role = "hub"
address = "10.8.0.2"

[[site]]
name = "haus2"
role = "spoke"
hub = "heim"
address = "10.8.0.11"

[[site]]
name = "haus3"
role = "spoke"
hub = "buero"
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
| `mesh.vlans` | – (required) | Shared VLANs: `"10-50"`, `"10,20,30-40"` or a list `[10, "20-30"]`. Only these VLANs are bridged; VLAN 1 and untagged traffic are not, unless a site maps them. |
| `mesh.mtu_probe_interval` | `5m` | Interval of the path MTU measurement (at least 10 s). |
| `mesh.mss` | unset | Fixed TCP MSS (536–1460) instead of measuring. |
| `mesh.commit_timeout` | `60s` | Time to confirm a change (10 s–1 h). |
| `mesh.control_port` | `51821` | Port of the daemon-to-daemon control channel. |
| `mesh.revision` | `0` | Revision counter used by the sync; raised automatically. |
| `site.name` | – | Unique, `a-z`, `0-9`, `-`, at most 12 characters (it becomes part of the interface name `gt-<name>`). |
| `site.role` | – | `hub` or `spoke`. |
| `site.hub` | – | For spokes: the name of their hub. |
| `site.address` | – | Unique IPv4 address of the site in the transport network. |
| `map.site` | – | Site the mapping applies to. |
| `map.local` | – | `"native"` (untagged) or a local VLAN ID. |
| `map.shared` | – | The VLAN ID in the shared network; must be in `vlans`. |

The configuration is checked when it is loaded; every problem is reported
with file and line, e.g.

```
mesh.toml:36: error: spoke "haus3": hub "buero2" does not exist
mesh.toml:49: error: map #2: shared VLAN 60 is not in mesh.vlans (10-50)
```

Rules: names and addresses are unique; every spoke names an existing hub;
hubs have no `hub`; there is at least one hub; shared VLANs of mappings are
in `vlans`; per site at most one native mapping, no local VLAN mapped twice
and no two local VLANs onto the same shared VLAN; `self` exists in the mesh;
unknown keys are errors (typos). Hubs are always fully meshed and every
spoke has exactly one tunnel, so the topology is loop-free by construction.

### VLAN mappings

* **Native**: untagged frames on the trunk of that site belong to the shared
  VLAN, and the shared VLAN leaves the trunk untagged.
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

Besides the topology check, the daemon watches the bridge's forwarding
database. If the same MAC address keeps moving between tunnel ports (5 times
within 10 s), it shuts the offending port down (spoke ports first), logs it
and shows it in the TUI. The port stays down until it is re-enabled with
`wanderlan unblock gt-<site>` (or `u` in the TUI) or the configuration
changes.

## Terminal UI

`wanderlan` without arguments starts the TUI (keyboard first, works over
SSH and in 80×24; `--mouse` enables the mouse). It talks to the daemon only
through `/run/wanderlan/wanderlan.sock`.

| Key | View |
|-----|------|
| `1` | Overview: all sites, live status per peer (last reply, RTT, path MTU, MSS, port state, MAC addresses, throughput, config in sync) |
| `2` | Sites: add (`n`), edit (`e`), delete (`d`); mesh settings (`m`), local settings (`l`) |
| `3` | VLANs: shared range (`v`), mapping table, conflicts are marked immediately |
| `4` | MAC table: filter (`/`), sort (`s`, `o`) |
| `5` | Log: live events, filter by level and peer |
| `6` | Setup wizard |

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

The socket can be changed with `--socket` or `WANDERLAN_SOCKET`.

## How it works

On each site the daemon creates, and removes again when it stops:

* `br0`: bridge, `vlan_filtering 1`, STP off, no default VLAN, no
  netfilter calls; marked with the interface alias `wanderlan`;
* `gt-<peer>`: GRETAP from the own to the peer's address, `ignore-df`,
  `nopmtudisc`, MTU 1500, marked with the alias `wanderlan`; on hubs, ports
  to other hubs are `isolated` (split horizon: isolated ports do not forward
  to each other, so spokes reach each other through their hub and everything
  else through the mesh, and no frame can circle);
* the configured VLANs on the trunk and the tunnels;
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
  `systemctl status wanderland`, `journalctl -u wanderland -e`.
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
  in `vlans` and that the switch port is a trunk with that VLAN.
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
```

`scripts/integration.sh` loads the required kernel modules into the host
kernel through a privileged helper container (on GitHub runners with sudo)
and runs the scenarios in `tests/integration/cases` on a topology of two
hubs, three spokes, a router standing in for the transport network and one
client per site (see `tests/integration/README.md`).

The workspace has three crates: `wanderlan-core` (configuration, topology,
desired state, reconciliation, system access behind traits with a netlink
and an in-memory implementation), `wanderland` (daemon) and `wanderlan`
(TUI and CLI).

## Maintainers: releases and the APT repository

One-time setup:

1. Create the signing key (RSA 4096) outside the repository:
   `scripts/gen-apt-key.sh ~/wanderlan-apt-key`, and commit its public part
   as `key.gpg` in the repository root (the release publishes it as
   `key.gpg` on GitHub Pages as well). Current key fingerprint:
   `F6C4 00CF 318B FD47 E3A6  B8A7 7BC2 D7AE 6699 82D4`.
2. Store `private.asc` as the repository secret `APT_GPG_PRIVATE_KEY`
   (Settings → Secrets and variables → Actions) and keep a backup offline.
3. After the first release, set GitHub Pages to "Deploy from a branch",
   branch `gh-pages`, folder `/`.

A release: set the version in `Cargo.toml`, update `CHANGELOG.md`, push the
tag `v<version>`. `release.yml` builds amd64 and arm64 natively, creates the
GitHub release with binaries, packages and `SHA256SUMS`, adds the packages to
the signed repository on `gh-pages` (older versions stay), waits until Pages
serves it, installs it on fresh Debian 13, Debian 12 and Ubuntu 24.04 with
the commands above and runs two hosts with the example configuration.
