//! Desired system state derived from the configuration.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{Ipv4Addr, SocketAddrV4};

use crate::BRIDGE_NAME;
use crate::config::{Config, LocalVid, NATIVE_VID, Network, TunnelMode};
use crate::forwarding;
use crate::identity::{PeerIdentity, SiteId};
use crate::mtu;
use crate::sys::breakout::{BreakoutSpec, Egress, Uplinks, conf};
use crate::sys::nft::{MssRule, PortDir, SplitRule};
use crate::sys::nft_ip::{FixedRule, LearnRule, LearnTable};
use crate::sys::tc::{Direction, TcAction, TcRule};
use crate::sys::{BridgeOptions, PortVlans, VlanFlags, VxlanParams};
use crate::topology::{self, TunnelKind};

/// Alias that marks interfaces created by wanderlan.
pub const OWNER_ALIAS: &str = "wanderlan";

/// The alias of a tunnel interface: the owner mark and the name of its peer,
/// so that `ip link` tells which site it leads to.
pub fn tunnel_alias(peer: &str) -> String {
    let peer: String = peer
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect();
    format!("{OWNER_ALIAS} {peer}")
}

/// True for the alias of an interface wanderlan made: [`OWNER_ALIAS`], or
/// that and a name ([`tunnel_alias`]).
pub fn is_owner_alias(alias: Option<&str>) -> bool {
    alias.is_some_and(|a| {
        a == OWNER_ALIAS
            || a.strip_prefix(OWNER_ALIAS)
                .is_some_and(|rest| rest.starts_with(' '))
    })
}
/// MTU of the tunnel interfaces: full Ethernet frames. On a smaller path
/// the outer packets of frames without DF are fragmented; for those with DF
/// the kernel answers "fragmentation needed" (VXLAN path MTU discovery).
pub const TUNNEL_MTU: u32 = 1500;

/// Prefix of the bridges of additional trunks (`wl-<trunk>`). The default
/// trunk is in [`BRIDGE_NAME`].
pub const SEGMENT_PREFIX: &str = "wl-";
/// Longest interface name Linux accepts.
const IFNAME_MAX: usize = 15;

/// Options wanderlan sets on its bridges.
pub const BRIDGE_OPTIONS: BridgeOptions = BridgeOptions {
    vlan_filtering: true,
    stp: false,
    default_pvid: 0,
    nf_call: false,
    vlan_stats: true,
};

/// A trunk port and its bridge: one segment of this site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrunkSpec {
    /// Interface name.
    pub name: String,
    /// The bridge of this trunk and its tunnels.
    pub bridge: String,
    /// VLAN membership.
    pub vlans: PortVlans,
    /// VLAN translation rules (tc), sorted.
    pub tc: Vec<TcRule>,
    /// Only the main port of the default segment must exist; a missing
    /// further port (e.g. an unplugged USB adapter) is left out.
    pub required: bool,
    /// Why the port is down (blocked by the loop guard), if it is.
    pub down_reason: Option<String>,
}

/// One tunnel port (VXLAN).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelSpec {
    /// Interface name (`vx` and the start of the peer's identity).
    pub name: String,
    /// Remote site.
    pub peer: SiteId,
    /// How the remote site is called (for messages).
    pub peer_name: String,
    /// The alias of the interface ([`tunnel_alias`]).
    pub alias: String,
    /// The bridge (segment) the tunnel belongs to: that of its trunk.
    pub bridge: String,
    /// VXLAN parameters.
    pub params: VxlanParams,
    /// Where the tunnel sends to; `None` until a client behind NAT has
    /// shown where its packets come from.
    pub remote: Option<SocketAddrV4>,
    /// Split horizon: isolated ports do not forward to each other. Every
    /// tunnel port is isolated, so frames from one tunnel never enter another.
    pub isolated: bool,
    /// Whether the port should be up.
    pub enabled: bool,
    /// Why it is down: blocked by the loop guard, or a site is offline.
    pub down_reason: Option<String>,
    /// VLAN membership: the VLANs both ends take part in.
    pub vlans: PortVlans,
}

/// Everything wanderlan manages on one site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Desired {
    /// Bridge options (the same for every bridge).
    pub bridge_options: BridgeOptions,
    /// The trunks, each with a bridge of its own: the default trunk (in
    /// [`BRIDGE_NAME`]) first, then the trunks of single tunnels by name.
    pub trunks: Vec<TrunkSpec>,
    /// Tunnels to the peer sites, sorted by name.
    pub tunnels: Vec<TunnelSpec>,
    /// Address of the site in the network.
    pub local_address: Ipv4Addr,
    /// TCP MSS clamping rules (nftables).
    pub mss: BTreeSet<MssRule>,
    /// Split horizon in nftables instead of `isolated` ports, where this
    /// site passes VLANs on between server tunnels (see [`forwarding`]).
    pub split: BTreeSet<SplitRule>,
    /// The UDP port of the tunnels.
    pub tunnel_port: u16,
    /// The control port (UDP probes, TCP sync) of the site.
    pub control_port: u16,
    /// Tunnels that cannot be built, by peer, with the reason.
    pub unbuilt: BTreeMap<SiteId, String>,
    /// The table `ip wanderlan` (see [`crate::sys::nft_ip`]): the clients
    /// whose endpoints are learned, the tunnel packets of the clients that
    /// skip connection tracking (only where it is loaded), the tunnels
    /// through NAT (their packets get the tunnel port as source port).
    pub learn: LearnTable,
    /// The local breakouts (see [`crate::breakout`]), one per segment with
    /// a tunnel and VLAN with a breakout.
    pub breakout: Vec<BreakoutSpec>,
}

impl Desired {
    /// Names of the bridges, each once (several trunk ports share one).
    pub fn bridges(&self) -> impl Iterator<Item = &str> {
        let mut seen = BTreeSet::new();
        self.trunks
            .iter()
            .map(|t| t.bridge.as_str())
            .filter(move |b| seen.insert(*b))
    }

    /// The default trunk.
    pub fn default_trunk(&self) -> Option<&TrunkSpec> {
        self.trunks.first()
    }

    /// The sysctls the breakouts set (to read before comparing).
    pub fn breakout_sysctls(&self) -> Vec<String> {
        let paths: BTreeSet<&String> = self
            .breakout
            .iter()
            .flat_map(|b| b.sysctls.keys())
            .collect();
        paths.into_iter().cloned().collect()
    }
}

/// The bridge of `trunk`: [`BRIDGE_NAME`] for the default trunk,
/// `wl-<trunk>` for every other (`wl-` and a hash if the name is too long).
pub fn bridge_for(trunk: &str, default_trunk: &str) -> String {
    if !default_trunk.is_empty() && trunk == default_trunk {
        return BRIDGE_NAME.to_string();
    }
    let name = format!("{SEGMENT_PREFIX}{trunk}");
    if name.len() <= IFNAME_MAX {
        return name;
    }
    // FNV-1a: stable across versions and platforms.
    let hash = trunk.bytes().fold(0x811c_9dc5_u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
    });
    format!("{SEGMENT_PREFIX}{:08x}", hash)
}

/// Runtime information that, besides the configuration, shapes the desired
/// state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Runtime {
    /// Tunnel interfaces shut down by the loop guard.
    pub blocked: BTreeSet<String>,
    /// Measured path MTU per peer site.
    pub pmtu: BTreeMap<SiteId, u16>,
    /// MTU of the interface the tunnel to a peer site leaves by: the path
    /// MTU is at most that, so it gives an MSS until the path is measured.
    pub underlay_mtu: BTreeMap<SiteId, u16>,
    /// VNIs of the tunnels to peers whose identity is known (both sites
    /// have one, the keys derive from them); the others use a VNI from the
    /// two names and come up without proof of identity.
    pub vnis: BTreeMap<SiteId, u32>,
    /// What the probes found about the identity of the peers with a key. A
    /// tunnel with a key comes up only once its peer is confirmed.
    pub identity: BTreeMap<SiteId, PeerIdentity>,
    /// Where the tunnel packets of the clients of this site come from (the
    /// NAT in front of them chooses the port), learned from their
    /// keepalives.
    pub endpoints: BTreeMap<SiteId, SocketAddrV4>,
    /// The clients of this site that it reaches directly, at this address
    /// (they passed the test of `wanderlan_core::nat`): their tunnels run
    /// direct.
    pub direct: BTreeMap<SiteId, Ipv4Addr>,
    /// The uplink of this site (a client) reaches it directly and runs the
    /// tunnel direct.
    pub uplink_direct: bool,
    /// Connection tracking is loaded (it would cost every tunnel packet).
    pub conntrack: bool,
    /// The interfaces the breakouts can leave by.
    pub uplinks: Uplinks,
    /// The current values of the sysctls the breakouts depend on (path
    /// below `/proc/sys/net`, see [`breakout_sysctl_paths`]).
    pub sysctls: BTreeMap<String, String>,
    /// The sysctls can be written (not in a container).
    pub sysctl_writable: bool,
    /// The kernel has no IPv6 (`ipv6.disable=1`): no IPv6 sysctls either.
    pub ipv6_disabled: bool,
    /// The ports (control, tunnel) the direct peers say they use now, in
    /// their probes: they win over their entries in the network, which
    /// learn a change only with the sync.
    pub peer_ports: BTreeMap<SiteId, (u16, u16)>,
}

impl Runtime {
    /// The control port of `peer`: the one it uses now, else its entry.
    pub fn control_port_of(&self, network: &Network, peer: SiteId) -> u16 {
        self.peer_ports
            .get(&peer)
            .map_or_else(|| network.control_port_of(peer), |p| p.0)
    }

    /// The tunnel port of `peer`: the one it uses now, else its entry.
    pub fn tunnel_port_of(&self, network: &Network, peer: SiteId) -> u16 {
        self.peer_ports
            .get(&peer)
            .map_or_else(|| network.tunnel_port_of(peer), |p| p.1)
    }

    /// How the tunnel `t` runs: servers among each other direct, a client
    /// tunnel direct once the server reached the client directly.
    pub fn mode(&self, t: &topology::Tunnel<'_>) -> TunnelMode {
        let direct = match t.kind {
            TunnelKind::Server => true,
            TunnelKind::Uplink => self.uplink_direct,
            TunnelKind::Client => self.direct.contains_key(&t.peer.id),
        };
        if direct {
            TunnelMode::Direct
        } else {
            TunnelMode::Nat
        }
    }
}

/// Which network of the box a desired state is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    /// Its tag in the names of its tunnels.
    pub tag: char,
    /// Its name, in the aliases of its tunnels (`<name>/<peer>`); `None`
    /// for the default network (`<peer>`).
    pub name: Option<String>,
}

impl Default for Scope {
    fn default() -> Self {
        Self {
            tag: topology::DEFAULT_TAG,
            name: None,
        }
    }
}

/// Computes the desired state of the default network.
pub fn derive(cfg: &Config, runtime: &Runtime) -> Desired {
    derive_in(&Scope::default(), cfg, runtime)
}

/// Computes the desired state of the network `scope` (only the default
/// network has its main trunk in [`BRIDGE_NAME`]).
pub fn derive_in(scope: &Scope, cfg: &Config, runtime: &Runtime) -> Desired {
    let tag = scope.tag;
    let blocked = &runtime.blocked;
    let network = &cfg.network;
    let me = cfg.self_id();
    let my_name = cfg.self_name();
    let local_address = cfg.self_site().map_or(Ipv4Addr::UNSPECIFIED, |s| s.address);

    let own = cfg
        .self_site()
        .map(|s| network.effective_vlans(s))
        .unwrap_or_default();
    // The site's own VLANs: on its trunk and towards its own clients only.
    let own_extra = cfg
        .self_site()
        .map(|s| network.own_vlans(s))
        .unwrap_or_default();
    let downwards: crate::config::VlanSet = own.iter().chain(own_extra.iter()).collect();
    // The native VLAN leaves the trunk untagged (and untagged frames belong
    // to it); on the tunnels every VLAN is tagged.
    let shared: PortVlans = downwards
        .iter()
        .map(|v| {
            let flags = if v == NATIVE_VID {
                VlanFlags::NATIVE
            } else {
                VlanFlags::TAGGED
            };
            (v, flags)
        })
        .collect();
    let (trunk_vlans, tc) = trunk_mapping(cfg, &shared);
    let default_trunk = if tag == topology::DEFAULT_TAG {
        cfg.trunk()
    } else {
        ""
    };

    let self_online = cfg.self_site().is_none_or(|s| s.online);
    // This site listens (and through NAT sends) on its own tunnel port; it
    // sends to the one of each peer.
    let tunnel_port = cfg.tunnel_port();
    let passing = forwarding::forwarding(network, me);
    let mut has_clients = false;
    let mut unbuilt = BTreeMap::new();
    let mut learn = BTreeSet::new();
    let mut vni_of: BTreeMap<u32, String> = BTreeMap::new();
    let mut tunnels: Vec<TunnelSpec> = Vec::new();
    for t in topology::tunnels_for(network, me) {
        let name = t.ifname(tag);
        let peer = t.peer.id;
        let label = network.label(peer);
        // What the tunnel carries: the VLANs both ends take part in (on
        // the way down also the own VLANs of the upper site), at most its
        // own limit.
        let carried = topology::tunnel_vlans(network, me, peer).unwrap_or_default();
        let keyed = runtime.vnis.get(&peer).copied();
        let vni = keyed.unwrap_or_else(|| fallback_vni(my_name, &t.peer.name));
        // One VNI per UDP port on a box (the kernel refuses a second).
        if let Some(other) = vni_of.get(&vni) {
            unbuilt.insert(
                peer,
                format!(
                    "its tunnel would get the same VNI {vni} as the one to \"{other}\" (by chance, one in 16 million); a `wanderlan reset` and a new join of \"{label}\" gives it another"
                ),
            );
            continue;
        }
        vni_of.insert(vni, label.clone());
        // Down while blocked by the loop guard, or while this site or the
        // peer is offline (set up but not taken online yet).
        let identity = runtime.identity.get(&peer).copied();
        let down_reason = if blocked.contains(&name) {
            Some("blocked by the loop guard".to_string())
        } else if !self_online {
            Some("this site is offline".to_string())
        } else if !t.peer.online {
            Some(format!("site \"{label}\" is offline"))
        } else if keyed.is_some() {
            identity_reason(&label, t.peer.address, identity.unwrap_or_default())
        } else {
            None
        };
        // Servers send directly to each other. Through NAT the client sends
        // from the tunnel port and its server answers where the client's
        // packets come from; a client its server reached directly gets its
        // packets there.
        let mode = runtime.mode(&t);
        has_clients |= t.kind == TunnelKind::Client;
        let remote = match (t.kind, mode) {
            (TunnelKind::Client, TunnelMode::Nat) => {
                // The set of a client is named by its tunnel: the name of
                // the client may not be its own.
                learn.insert(LearnRule {
                    client: name.clone(),
                    vni,
                    port: tunnel_port,
                });
                runtime.endpoints.get(&peer).copied()
            }
            (TunnelKind::Client, TunnelMode::Direct) => runtime
                .direct
                .get(&peer)
                .map(|ip| SocketAddrV4::new(*ip, runtime.tunnel_port_of(network, peer))),
            _ => Some(SocketAddrV4::new(
                t.peer.address,
                runtime.tunnel_port_of(network, peer),
            )),
        };
        let down_reason = down_reason.or_else(|| {
            remote.is_none().then(|| {
                format!(
                    "waiting for client \"{label}\" to connect (its tunnel packets, UDP {tunnel_port})"
                )
            })
        });
        tunnels.push(TunnelSpec {
            enabled: down_reason.is_none(),
            down_reason,
            name,
            peer,
            alias: match &scope.name {
                Some(network) => tunnel_alias(&format!("{network}/{label}")),
                None => tunnel_alias(&label),
            },
            peer_name: label,
            bridge: bridge_for(cfg.trunk_for(peer), default_trunk),
            params: VxlanParams {
                vni,
                port: tunnel_port,
                source_port: (mode == TunnelMode::Nat).then_some(tunnel_port),
            },
            remote,
            isolated: passing.isolated.contains(&peer),
            vlans: carried.iter().map(|v| (v, VlanFlags::TAGGED)).collect(),
        });
    }

    // Every trunk port carries the same VLANs and mappings; the ports of the
    // default segment always exist, those of a tunnel's own segment while
    // the tunnel does. The first port of a list names its segment's bridge.
    let mut segments: Vec<&[String]> = vec![cfg.local.trunk.as_slice()];
    let mut extra: Vec<&[String]> = tunnels
        .iter()
        .map(|t| cfg.trunk_ports_for(t.peer))
        .filter(|ports| *ports != cfg.local.trunk.as_slice())
        .collect();
    extra.sort_unstable();
    extra.dedup();
    segments.extend(extra);
    let mut trunks: Vec<TrunkSpec> = Vec::new();
    for (s, ports) in segments.into_iter().enumerate() {
        let bridge = bridge_for(ports.first().map_or("", String::as_str), default_trunk);
        for (i, port) in ports.iter().enumerate() {
            if trunks.iter().any(|t| t.name == *port) {
                continue;
            }
            trunks.push(TrunkSpec {
                name: port.clone(),
                bridge: bridge.clone(),
                vlans: trunk_vlans.clone(),
                tc: tc.clone(),
                required: s == 0 && i == 0,
                down_reason: blocked
                    .contains(port)
                    .then(|| "blocked by the loop guard".to_string()),
            });
        }
    }

    // The packets of the client tunnels skip connection tracking on their
    // server (a client may need it: its firewall lets the answers in).
    let notrack = (runtime.conntrack && has_clients)
        .then_some(tunnel_port)
        .into_iter()
        .collect();
    let breakout = breakouts(cfg, runtime, &trunks, &tunnels, &trunk_vlans);
    let fixed = tunnels
        .iter()
        .filter(|t| t.params.source_port.is_some())
        .map(|t| FixedRule {
            port: tunnel_port,
            vni: t.params.vni,
        })
        .collect();
    Desired {
        tunnel_port,
        control_port: cfg.control_port(),
        unbuilt,
        learn: LearnTable {
            rules: learn,
            notrack,
            fixed,
        },
        bridge_options: BRIDGE_OPTIONS,
        trunks,
        mss: mss_rules(cfg, &tunnels, runtime),
        split: passing
            .split
            .iter()
            .map(|(peers, vlans)| SplitRule {
                ports: peers
                    .iter()
                    .filter_map(|p| network.site(*p).map(|s| topology::tunnel_name(tag, s)))
                    .collect(),
                vlans: vlans.ranges(),
            })
            .collect(),
        tunnels,
        local_address,
        breakout,
    }
}

/// The local breakouts of this site: for every segment with its tunnel to
/// its server and every VLAN of the site with a breakout. Only a client
/// breaks out; a server (and a client + server in the segments with only
/// its own clients) is where the gateways of its devices are.
fn breakouts(
    cfg: &Config,
    runtime: &Runtime,
    trunks: &[TrunkSpec],
    tunnels: &[TunnelSpec],
    trunk_vlans: &PortVlans,
) -> Vec<BreakoutSpec> {
    let Some(site) = cfg.self_site() else {
        return Vec::new();
    };
    let Some(server) = site.uplink else {
        return Vec::new();
    };
    if site.breakout.is_empty() {
        return Vec::new();
    }
    // The segments in the order of the trunks (the default one first).
    let mut segments: Vec<(&str, Vec<String>)> = Vec::new();
    for t in trunks {
        match segments.iter_mut().find(|(b, _)| *b == t.bridge) {
            Some((_, ports)) => ports.push(t.name.clone()),
            None => segments.push((t.bridge.as_str(), vec![t.name.clone()])),
        }
    }
    let trunk_ports: BTreeSet<&str> = trunks.iter().map(|t| t.name.as_str()).collect();
    let mut out = Vec::new();
    for (segment, (bridge, ports)) in segments.iter().enumerate() {
        if !tunnels
            .iter()
            .any(|t| t.bridge == *bridge && t.peer == server)
        {
            continue;
        }
        for (vid, b) in &site.breakout {
            if !trunk_vlans.contains_key(vid) {
                continue;
            }
            let Some(id) = crate::breakout::id(segment, *vid) else {
                continue;
            };
            let (router, port) = crate::breakout::names(segment, *vid);
            let uplink = b.uplink.as_deref();
            let check = |v6| -> Result<Egress, String> {
                if let Some(dev) = uplink
                    && (trunk_ports.contains(dev) || is_own_interface(dev))
                {
                    return Err(format!(
                        "{dev} is a trunk port or an interface of wanderlan, not an uplink"
                    ));
                }
                if v6 && runtime.ipv6_disabled {
                    return Err("IPv6 is disabled on this box".into());
                }
                let egress = egress(v6, uplink, &runtime.uplinks)?;
                forwarding(v6, egress.dev(), &router, runtime)?;
                Ok(egress)
            };
            let v4 = check(false);
            let v6 = check(true);
            let sysctls = breakout_sysctls(&v4, &v6, &router, &port, runtime);
            out.push(BreakoutSpec {
                bridge: (*bridge).to_string(),
                vid: *vid,
                id,
                mac: crate::breakout::mac(segment, *vid),
                router,
                port,
                trunks: ports.clone(),
                except: b.except.clone(),
                v4,
                v6,
                sysctls,
            });
        }
    }
    out
}

/// True for the interfaces wanderlan makes itself.
fn is_own_interface(name: &str) -> bool {
    name == BRIDGE_NAME
        || name.starts_with(SEGMENT_PREFIX)
        || topology::is_tunnel_name(name)
        || crate::breakout::is_breakout_name(name)
}

/// How one family leaves: by the default route (`uplink` `None`) or out of
/// the chosen interface, or why it cannot.
fn egress(v6: bool, uplink: Option<&str>, uplinks: &Uplinks) -> Result<Egress, String> {
    let family = if v6 { "IPv6" } else { "IPv4" };
    let auto = if v6 { &uplinks.auto6 } else { &uplinks.auto4 };
    let dev = match uplink {
        Some(dev) => dev.to_string(),
        None => auto
            .clone()
            .ok_or_else(|| format!("this box has no {family} default route"))?,
    };
    let Some(info) = uplinks.by_name.get(&dev) else {
        return Err(format!("the uplink {dev} does not exist"));
    };
    if !info.up {
        return Err(format!("the uplink {dev} is down"));
    }
    let addressed = if v6 {
        !info.v6.is_empty()
    } else {
        !info.v4.is_empty()
    };
    if !addressed {
        return Err(format!(
            "{dev} has no {}{family} address",
            if v6 { "global " } else { "" }
        ));
    }
    if uplink.is_none() {
        return Ok(Egress::Main { dev });
    }
    let default = if v6 {
        info.default6.map(|g| g.map(std::net::IpAddr::V6))
    } else {
        info.default4.map(|g| g.map(std::net::IpAddr::V4))
    };
    match default {
        Some(gateway) => Ok(Egress::Fixed { dev, gateway }),
        None if info.pointopoint => Ok(Egress::Fixed { dev, gateway: None }),
        None => Err(format!(
            "{dev} has no {family} default route (an uplink needs one of its own, with any metric)"
        )),
    }
}

/// Whether the box can forward a family for the router `router` leaving by
/// `dev`: it is on already, or the sysctls can be written.
fn forwarding(v6: bool, dev: &str, router: &str, runtime: &Runtime) -> Result<(), String> {
    if runtime.sysctl_writable {
        return Ok(());
    }
    let value = |path: &str| runtime.sysctls.get(path).map(String::as_str);
    // A new router takes the default of new interfaces.
    let own_or_default = |key: &str, v6: bool| {
        value(&conf(v6, router, key)).or_else(|| value(&conf(v6, "default", key)))
    };
    if v6 {
        if value(&conf(true, "all", "forwarding")) != Some("1") {
            return Err(
                "IPv6 forwarding is off and the sysctls cannot be written (net.ipv6.conf.all.forwarding = 1 is needed)"
                    .into(),
            );
        }
        if own_or_default("disable_ipv6", true) == Some("1") {
            return Err(
                "IPv6 is disabled on new interfaces and the sysctls cannot be written (net.ipv6.conf.default.disable_ipv6 = 0 is needed)"
                    .into(),
            );
        }
    } else {
        if value(&conf(false, dev, "forwarding")) != Some("1")
            || own_or_default("forwarding", false) != Some("1")
        {
            return Err(format!(
                "IPv4 forwarding is off and the sysctls cannot be written (net.ipv4.conf.{dev}.forwarding = 1 and net.ipv4.conf.default.forwarding = 1 are needed)"
            ));
        }
        // The kernel takes the stricter of `all` and the interface: strict
        // source checks drop what the devices send (their addresses are
        // reached through the router, not by a route of the box).
        let level = |v: Option<&str>| v.and_then(|v| v.parse::<u8>().ok()).unwrap_or(0);
        let all = level(value(&conf(false, "all", "rp_filter")));
        let router_rp = all.max(level(own_or_default("rp_filter", false)));
        if router_rp == 1 {
            return Err(
                "strict reverse path filtering is on and the sysctls cannot be written (net.ipv4.conf.all.rp_filter and .default.rp_filter = 2 are needed)"
                    .into(),
            );
        }
    }
    Ok(())
}

/// The sysctls of a breakout: forwarding on the router and the uplink,
/// IPv6 forwarding; where the sysctls can be written also what keeps the
/// router quiet and the uplink taking its IPv6 router advertisements.
fn breakout_sysctls(
    v4: &Result<Egress, String>,
    v6: &Result<Egress, String>,
    router: &str,
    port: &str,
    runtime: &Runtime,
) -> BTreeMap<String, String> {
    let writable = runtime.sysctl_writable;
    let value = |path: &str| runtime.sysctls.get(path).map(String::as_str);
    let mut out = BTreeMap::new();
    let mut set = |path: String, v: &str| {
        out.insert(path, v.to_string());
    };
    if let Ok(egress) = v4 {
        let dev = egress.dev();
        set(conf(false, dev, "forwarding"), "1");
        set(conf(false, router, "forwarding"), "1");
        if writable {
            // Loose source checks: the devices are reached through the
            // router, the internet through the uplink.
            set(conf(false, router, "rp_filter"), "2");
            // The router answers no ARP request (it asks only).
            set(conf(false, router, "arp_ignore"), "8");
            set(conf(false, router, "arp_announce"), "2");
            let strict = |d: &str| value(&conf(false, d, "rp_filter")) == Some("1");
            let all = value(&conf(false, "all", "rp_filter"))
                .and_then(|v| v.parse::<u8>().ok())
                .unwrap_or(0);
            if matches!(egress, Egress::Fixed { .. }) && (strict(dev) || all == 1) {
                // The answers of a second uplink come in where the main
                // table does not lead.
                set(conf(false, dev, "rp_filter"), "2");
            }
        }
    }
    if v6.is_ok() {
        set(conf(true, "all", "forwarding"), "1");
        set(conf(true, router, "disable_ipv6"), "0");
        if writable {
            set(conf(true, router, "accept_ra"), "0");
            set(conf(true, router, "autoconf"), "0");
            // Forwarding on: router advertisements count only with 2, on
            // every interface that takes them now (else their default
            // routes go when forwarding is switched on), not only on the
            // uplink of the breakout.
            let taking: Vec<String> = runtime
                .sysctls
                .iter()
                .filter(|(path, v)| {
                    v.as_str() == "1"
                        && path.starts_with("ipv6/conf/")
                        && path.ends_with("/accept_ra")
                })
                .map(|(path, _)| path.clone())
                .filter(|path| {
                    path.split('/')
                        .nth(2)
                        .is_some_and(|d| d != "all" && d != "default" && d != router)
                })
                .collect();
            for path in taking {
                set(path, "2");
            }
        }
    }
    if writable && (v4.is_ok() || v6.is_ok()) && !runtime.ipv6_disabled {
        // The port end has no address of its own.
        set(conf(true, port, "disable_ipv6"), "1");
    }
    out
}

/// The sysctls the breakouts depend on, for the given interfaces (the
/// uplinks and the routers there are): what the daemon reads into
/// [`Runtime::sysctls`].
pub fn breakout_sysctl_paths<'a>(devs: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut out = BTreeSet::new();
    for dev in devs.into_iter().chain(["all", "default"]) {
        for key in ["forwarding", "rp_filter"] {
            out.insert(conf(false, dev, key));
        }
        for key in ["forwarding", "accept_ra", "disable_ipv6"] {
            out.insert(conf(true, dev, key));
        }
    }
    out.into_iter().collect()
}

/// Why a tunnel with a key is down: its peer has not proven its identity.
fn identity_reason(peer: &str, address: Ipv4Addr, state: PeerIdentity) -> Option<String> {
    match state {
        PeerIdentity::Confirmed => None,
        PeerIdentity::Waiting => Some(format!(
            "the identity of site \"{peer}\" is not confirmed yet (no authenticated answer to the control probes)"
        )),
        PeerIdentity::WrongKey => Some(format!(
            "the box at {address} answers with another key than site \"{peer}\" has (another box at that address, or one that was reset?)"
        )),
    }
}

/// The VNI of a tunnel to a peer whose identity is not known (yet): the
/// same at both ends, from the two names (FNV-1a, 24 bits, never 0).
fn fallback_vni(a: &str, b: &str) -> u32 {
    let (low, high) = if a < b { (a, b) } else { (b, a) };
    let hash = format!("wanderlan {low}\0{high}")
        .bytes()
        .fold(0x811c_9dc5_u32, |h, byte| {
            (h ^ u32::from(byte)).wrapping_mul(0x0100_0193)
        });
    (hash & 0x00ff_ffff).max(1)
}

/// MSS clamping per tunnel port: a fixed value from the configuration, or
/// the value derived from the measured path MTU of the peer. Ports without a
/// measurement use the smaller of the smallest measured value and the MTU of
/// the interface towards the peer (the path cannot be larger), so no TCP
/// connection starts unclamped. Rules apply to SYNs entering and leaving
/// through the port, so both ends of a connection see the smaller value.
fn mss_rules(cfg: &Config, tunnels: &[TunnelSpec], runtime: &Runtime) -> BTreeSet<MssRule> {
    let measured: BTreeMap<&str, u16> = tunnels
        .iter()
        .filter_map(|t| runtime.pmtu.get(&t.peer).map(|p| (t.name.as_str(), *p)))
        .collect();
    // The smallest measured path, for tunnels not measured yet.
    let smallest = measured.values().copied().min();
    let mut out = BTreeSet::new();
    for t in tunnels {
        let fallback = smallest.map(mtu::mss_for_pmtu);
        let underlay = runtime
            .underlay_mtu
            .get(&t.peer)
            .map(|m| mtu::mss_for_pmtu(*m));
        let preliminary = match (fallback, underlay) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let mss = cfg
            .mss()
            .or_else(|| measured.get(t.name.as_str()).map(|p| mtu::mss_for_pmtu(*p)))
            .or(preliminary);
        let Some(mss) = mss else { continue };
        for dir in [PortDir::In, PortDir::Out] {
            for ipv6 in [false, true] {
                out.insert(MssRule {
                    port: t.name.clone(),
                    dir,
                    ipv6,
                    mss: if ipv6 { mtu::mss_ipv6(mss) } else { mss },
                });
            }
        }
    }
    out
}

/// VLAN membership and translation rules of the trunk.
///
/// * A native mapping makes the shared VLAN the untagged VLAN of the trunk.
/// * A translation local L -> shared S rewrites L to S on ingress and S to L
///   on egress. The trunk is a member of S (frames arrive already rewritten).
/// * A translated local VLAN L is not available as shared VLAN L on this
///   trunk, unless L is itself the target of another translation.
/// * Frames arriving tagged with a translation target S are dropped, unless S
///   is itself a translated local VLAN; otherwise local VLAN S and the
///   translated VLAN would merge into one.
fn trunk_mapping(cfg: &Config, shared: &PortVlans) -> (PortVlans, Vec<TcRule>) {
    let me = cfg.self_id();
    let mut native = None;
    let mut translations: Vec<(u16, u16)> = Vec::new();
    for map in cfg.network.maps_of(me) {
        match map.local {
            LocalVid::Native => native = Some(map.shared),
            LocalVid::Vid(local) => translations.push((local, map.shared)),
        }
    }
    let locals: BTreeSet<u16> = translations.iter().map(|(l, _)| *l).collect();
    let targets: BTreeSet<u16> = translations.iter().map(|(_, s)| *s).collect();

    let mut vlans: PortVlans = shared
        .iter()
        .filter(|(vid, _)| !locals.contains(vid) || targets.contains(vid))
        .map(|(vid, flags)| (*vid, *flags))
        .collect();
    if let Some(vid) = native {
        vlans.insert(vid, VlanFlags::NATIVE);
    }

    let mut rules = Vec::new();
    for (local, target) in &translations {
        rules.push(TcRule {
            direction: Direction::Ingress,
            vid: *local,
            action: TcAction::Translate(*target),
        });
        rules.push(TcRule {
            direction: Direction::Egress,
            vid: *target,
            action: TcAction::Translate(*local),
        });
    }
    // Tagged frames with a target VID (translated, or untagged native) do
    // not enter it from the trunk as well: they would merge with it.
    for target in targets
        .iter()
        .copied()
        .chain(native)
        .filter(|t| !locals.contains(t))
        .collect::<BTreeSet<u16>>()
    {
        rules.push(TcRule {
            direction: Direction::Ingress,
            vid: target,
            action: TcAction::Drop,
        });
    }
    rules.sort();
    (vlans, rules)
}

#[cfg(test)]
impl Desired {
    /// The default trunk (tests only).
    fn trunk(&self) -> &TrunkSpec {
        &self.trunks[0]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Loaded, load};

    const NETWORK: &str = include_str!("config/testdata/network-v1.toml");
    /// A server with a client behind NAT.
    const NETWORK_CLIENTS: &str = "[network]\nvlans = \"10-50\"\n\n[site.heim]\naddress = \"10.8.0.1\"\n\n[site.nat1]\naddress = \"10.8.0.30\"\nserver = \"heim\"\n";

    fn config(self_name: &str) -> Config {
        let local = format!("self = \"{self_name}\"\ntrunk = \"ens19\"\n");
        match load(NETWORK, &local).unwrap().loaded {
            Loaded::Configured(cfg) => cfg,
            Loaded::Unconfigured { .. } => panic!("configured"),
        }
    }

    #[test]
    fn every_site_gets_an_isolated_tunnel_to_every_other_site() {
        let d = derive(&config("heim"), &Runtime::default());
        assert_eq!(d.bridges().collect::<Vec<_>>(), ["br0"]);
        assert_eq!(d.local_address, Ipv4Addr::new(10, 8, 0, 1));
        let at = |a, b, c, d| Some(SocketAddrV4::new(Ipv4Addr::new(a, b, c, d), 51822));
        let summary: Vec<_> = d
            .tunnels
            .iter()
            .map(|t| (t.name.as_str(), t.isolated, t.remote))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("vx-buero", true, at(10, 8, 0, 2)),
                ("vx-haus2", true, at(10, 8, 0, 11)),
                ("vx-haus3", true, at(10, 8, 0, 12)),
            ]
        );
        // Between servers: direct, a source port per flow.
        for t in &d.tunnels {
            assert_eq!(t.params.port, 51822);
            assert_eq!(t.params.source_port, None);
        }
        // Without identities both ends take the VNI from the two names.
        let buero = derive(&config("buero"), &Runtime::default());
        let vni = |d: &Desired, peer: &str| {
            d.tunnels
                .iter()
                .find(|t| t.peer_name == peer)
                .unwrap()
                .params
                .vni
        };
        assert_eq!(vni(&d, "buero"), vni(&buero, "heim"));
        assert_ne!(vni(&d, "buero"), vni(&d, "haus2"));
        assert!(d.unbuilt.is_empty());
        assert_eq!(d.trunk().vlans.len(), 41);
        assert!(d.trunk().vlans.values().all(|f| *f == VlanFlags::TAGGED));
        assert!(!d.trunk().vlans.contains_key(&1));
    }

    #[test]
    fn each_site_listens_on_its_own_tunnel_port_and_sends_to_the_peers() {
        let mut cfg = config("heim");
        for site in &mut cfg.network.sites {
            match site.name.as_str() {
                "heim" => site.ports.control = Some(52821),
                "buero" => site.ports.tunnel = Some(52833),
                _ => {}
            }
        }
        let d = derive(&cfg, &Runtime::default());
        assert_eq!(d.tunnel_port, 52822, "the one after its own control port");
        let t = d.tunnels.iter().find(|t| t.peer_name == "buero").unwrap();
        assert_eq!(t.params.port, 52822);
        assert_eq!(t.remote.map(|r| r.port()), Some(52833));
        let other = d.tunnels.iter().find(|t| t.peer_name == "haus3").unwrap();
        assert_eq!(other.remote.map(|r| r.port()), Some(51822));
        // A peer that says it uses another port now gets its packets there.
        let haus3 = cfg.network.named("haus3").next().unwrap().id;
        let runtime = Runtime {
            peer_ports: [(haus3, (53000, 53001))].into(),
            ..Runtime::default()
        };
        let d = derive(&cfg, &runtime);
        let other = d.tunnels.iter().find(|t| t.peer_name == "haus3").unwrap();
        assert_eq!(other.remote.map(|r| r.port()), Some(53001));
    }

    #[test]
    fn a_client_behind_nat_is_reached_where_its_packets_come_from() {
        let text =
            format!("{NETWORK_CLIENTS}\n[site.lan]\naddress = \"10.8.0.31\"\nserver = \"heim\"\n");
        let cfg = |me: &str| match load(&text, &format!("self = \"{me}\"\ntrunk = \"ens19\"\n"))
            .unwrap()
            .loaded
        {
            Loaded::Configured(cfg) => cfg,
            Loaded::Unconfigured { .. } => panic!("configured"),
        };
        let tunnel = |d: &Desired, peer: &str| {
            d.tunnels
                .iter()
                .find(|t| t.peer_name == peer)
                .unwrap()
                .clone()
        };
        // "lan" passed the test: its server reached it at 10.8.0.32.
        let lan = Ipv4Addr::new(10, 8, 0, 32);
        let mut runtime = Runtime {
            direct: [("lan".into(), lan)].into(),
            ..Runtime::default()
        };
        let server = derive(&cfg("heim"), &runtime);
        // nat (where every client starts): no remote until the client was
        // seen, a fixed source port, learned by its VNI.
        let nat = tunnel(&server, "nat1");
        assert_eq!(nat.remote, None);
        assert!(!nat.enabled);
        assert!(
            nat.down_reason
                .as_deref()
                .unwrap()
                .contains("waiting for client \"nat1\"")
        );
        assert_eq!(nat.params.source_port, Some(51822));
        assert_eq!(
            server
                .learn
                .rules
                .iter()
                .map(|r| (r.client.as_str(), r.vni))
                .collect::<Vec<_>>(),
            [("vx-nat1", nat.params.vni)]
        );
        // direct: straight to where it was reached, a port per flow,
        // nothing learned.
        let direct = tunnel(&server, "lan");
        assert_eq!(direct.remote, Some(SocketAddrV4::new(lan, 51822)));
        assert_eq!(direct.params.source_port, None);
        assert!(direct.enabled);
        let endpoint = SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 7), 40001);
        runtime.endpoints.insert("nat1".into(), endpoint);
        let server = derive(&cfg("heim"), &runtime);
        assert_eq!(tunnel(&server, "nat1").remote, Some(endpoint));
        assert!(tunnel(&server, "nat1").enabled);
        // The clients follow what the test found towards their server.
        let client = derive(&cfg("nat1"), &Runtime::default());
        let up = tunnel(&client, "heim");
        assert_eq!(up.params.source_port, Some(51822));
        assert_eq!(
            up.remote,
            Some(SocketAddrV4::new(Ipv4Addr::new(10, 8, 0, 1), 51822))
        );
        assert_eq!(up.params.vni, nat.params.vni, "the same VNI at both ends");
        assert!(client.learn.rules.is_empty());
        assert_eq!(
            client.learn.fixed,
            [FixedRule {
                port: 51822,
                vni: up.params.vni
            }]
            .into()
        );
        let reached = Runtime {
            uplink_direct: true,
            ..Runtime::default()
        };
        let lan_side = derive(&cfg("lan"), &reached);
        assert_eq!(tunnel(&lan_side, "heim").params.source_port, None);
        assert!(lan_side.learn.fixed.is_empty());
    }

    #[test]
    fn a_second_tunnel_with_the_same_vni_is_not_built() {
        let mut runtime = Runtime::default();
        runtime.vnis.insert("buero".into(), 77);
        runtime.vnis.insert("haus2".into(), 77);
        let d = derive(&config("heim"), &runtime);
        assert_eq!(
            d.tunnels
                .iter()
                .map(|t| t.peer_name.as_str())
                .collect::<Vec<_>>(),
            ["buero", "haus3"]
        );
        assert!(d.unbuilt[&"haus2".into()].contains("same VNI 77 as the one to \"buero\""));
    }

    #[test]
    fn a_tunnel_carries_the_vlans_both_sites_take_part_in() {
        let heim = derive(&config("heim"), &Runtime::default());
        let vids = |d: &Desired, port: &str| -> Vec<u16> {
            d.tunnels
                .iter()
                .find(|t| t.name == port)
                .map(|t| t.vlans.keys().copied().collect())
                .unwrap_or_default()
        };
        assert_eq!(vids(&heim, "vx-buero").len(), 41);
        assert_eq!(vids(&heim, "vx-haus2"), vec![20, 30]);
        let haus2 = derive(&config("haus2"), &Runtime::default());
        assert_eq!(
            haus2.trunk().vlans.keys().copied().collect::<Vec<_>>(),
            vec![20, 30]
        );
        assert_eq!(vids(&haus2, "vx-heim"), vec![20, 30]);
    }

    #[test]
    fn offline_sites_have_their_tunnels_down() {
        let network = NETWORK.replace(
            "name = \"buero\"\naddress = \"10.8.0.2\"\n",
            "name = \"buero\"\naddress = \"10.8.0.2\"\nonline = false\n",
        );
        let at = |site: &str| {
            let local = format!("self = \"{site}\"\ntrunk = \"ens19\"\n");
            let Loaded::Configured(cfg) = load(&network, &local).unwrap().loaded else {
                panic!("configured");
            };
            derive(&cfg, &Runtime::default())
                .tunnels
                .iter()
                .map(|t| (t.name.clone(), t.enabled))
                .collect::<Vec<_>>()
        };
        // At heim only the tunnel to buero is down; at buero all are.
        assert_eq!(
            at("heim"),
            vec![
                ("vx-buero".to_string(), false),
                ("vx-haus2".to_string(), true),
                ("vx-haus3".to_string(), true),
            ]
        );
        assert!(at("buero").iter().all(|(_, enabled)| !enabled));
    }

    #[test]
    fn native_vlan_is_untagged_on_the_trunk_and_tagged_in_tunnels() {
        // All VLANs: VLAN 1 (native) is untagged on the trunk; haus3 maps its
        // untagged traffic to VLAN 20, so the native VLAN does not reach it.
        let network = NETWORK.replace("vlans = \"10-50\"", "vlans = \"all\"");
        let local = "self = \"heim\"\ntrunk = \"ens19\"\n";
        let Loaded::Configured(cfg) = load(&network, local).unwrap().loaded else {
            panic!("configured");
        };
        let d = derive(&cfg, &Runtime::default());
        assert_eq!(d.trunk().vlans.get(&1), Some(&VlanFlags::NATIVE));
        assert_eq!(d.trunk().vlans.get(&4094), Some(&VlanFlags::TAGGED));
        assert_eq!(d.trunk().vlans.len(), 4094);
        let tunnel = |name: &str| {
            d.tunnels
                .iter()
                .find(|t| t.name == name)
                .map(|t| t.vlans.clone())
                .unwrap_or_default()
        };
        assert_eq!(tunnel("vx-buero").get(&1), Some(&VlanFlags::TAGGED));
        assert_eq!(tunnel("vx-buero").len(), 4094);
        assert_eq!(tunnel("vx-haus3").get(&1), None, "haus3 maps native to 20");
        assert_eq!(tunnel("vx-haus3").len(), 4093);

        let local = "self = \"haus3\"\ntrunk = \"ens19\"\n";
        let Loaded::Configured(cfg) = load(&network, local).unwrap().loaded else {
            panic!("configured");
        };
        let d = derive(&cfg, &Runtime::default());
        assert_eq!(d.trunk().vlans.get(&1), None);
        assert_eq!(d.trunk().vlans.get(&20), Some(&VlanFlags::NATIVE));
    }

    #[test]
    fn native_mapping_and_translation_shape_the_trunk() {
        let d = derive(&config("haus3"), &Runtime::default());
        assert_eq!(d.trunk().vlans.get(&20), Some(&VlanFlags::NATIVE));
        assert_eq!(d.trunk().vlans.get(&30), Some(&VlanFlags::TAGGED));
        assert!(!d.trunk().vlans.contains_key(&130));
        assert_eq!(d.tunnels.len(), 3);
        assert!(d.tunnels.iter().all(|t| t.isolated));
    }

    #[test]
    fn translation_rules_for_haus3() {
        let d = derive(&config("haus3"), &Runtime::default());
        let rules: Vec<String> = d.trunk().tc.iter().map(TcRule::cookie).collect();
        // Native is 20 there: tagged 20 from the trunk is dropped too.
        assert_eq!(
            rules,
            vec!["wl:i20x", "wl:i30x", "wl:i130>30", "wl:e30>130"]
        );
        assert!(
            derive(&config("heim"), &Runtime::default())
                .trunk()
                .tc
                .is_empty()
        );
    }

    #[test]
    fn chained_translations_keep_targets_on_the_trunk() {
        // 130 -> 30 and 30 -> 40 at the same site: local 30 is translated, but
        // shared 30 is still needed as target of local 130.
        let network = NETWORK.replace("local = \"native\"\nshared = 20", "local = 30\nshared = 40");
        let local = "self = \"haus3\"\ntrunk = \"ens19\"\n";
        let Loaded::Configured(cfg) = load(&network, local).unwrap().loaded else {
            panic!("configured");
        };
        let d = derive(&cfg, &Runtime::default());
        assert_eq!(d.trunk().vlans.get(&30), Some(&VlanFlags::TAGGED));
        assert_eq!(d.trunk().vlans.get(&40), Some(&VlanFlags::TAGGED));
        let rules: Vec<String> = d.trunk().tc.iter().map(TcRule::cookie).collect();
        assert_eq!(
            rules,
            vec![
                "wl:i30>40",
                "wl:i40x",
                "wl:i130>30",
                "wl:e30>130",
                "wl:e40>30"
            ]
        );
    }

    #[test]
    fn translated_local_inside_shared_range_is_removed_from_trunk() {
        let network = NETWORK.replace("local = 130", "local = 15");
        let local = "self = \"haus3\"\ntrunk = \"ens19\"\n";
        let Loaded::Configured(cfg) = load(&network, local).unwrap().loaded else {
            panic!("configured");
        };
        let d = derive(&cfg, &Runtime::default());
        assert!(!d.trunk().vlans.contains_key(&15));
        assert_eq!(d.trunk().vlans.get(&30), Some(&VlanFlags::TAGGED));
    }

    #[test]
    fn mss_from_measurements_with_minimum_fallback() {
        let mut runtime = Runtime::default();
        assert!(derive(&config("heim"), &runtime).mss.is_empty());
        runtime.pmtu.insert("buero".into(), 1400);
        let d = derive(&config("heim"), &runtime);
        let rules: Vec<String> = d.mss.iter().map(MssRule::comment).collect();
        assert_eq!(
            rules,
            vec![
                "wl mss iif vx-buero ip 1306",
                "wl mss iif vx-buero ip6 1286",
                "wl mss oif vx-buero ip 1306",
                "wl mss oif vx-buero ip6 1286",
                "wl mss iif vx-haus2 ip 1306",
                "wl mss iif vx-haus2 ip6 1286",
                "wl mss oif vx-haus2 ip 1306",
                "wl mss oif vx-haus2 ip6 1286",
                "wl mss iif vx-haus3 ip 1306",
                "wl mss iif vx-haus3 ip6 1286",
                "wl mss oif vx-haus3 ip 1306",
                "wl mss oif vx-haus3 ip6 1286",
            ]
        );
        runtime.pmtu.insert("haus2".into(), 1500);
        let d = derive(&config("heim"), &runtime);
        assert!(
            d.mss
                .iter()
                .any(|r| r.comment() == "wl mss oif vx-haus2 ip 1406")
        );
        assert!(
            d.mss
                .iter()
                .any(|r| r.comment() == "wl mss oif vx-buero ip 1306")
        );
    }

    #[test]
    fn the_interface_towards_a_peer_gives_an_mss_before_it_is_measured() {
        let mut runtime = Runtime::default();
        // The tunnel to buero leaves over a VPN with MTU 1420.
        runtime.underlay_mtu.insert("buero".into(), 1420);
        let d = derive(&config("heim"), &runtime);
        let buero: Vec<String> = d
            .mss
            .iter()
            .filter(|r| r.port == "vx-buero")
            .map(MssRule::comment)
            .collect();
        assert!(
            buero.contains(&"wl mss oif vx-buero ip 1326".to_string()),
            "{buero:?}"
        );
        // Measured, the measurement counts; others take the smaller value.
        runtime.pmtu.insert("buero".into(), 1400);
        runtime.underlay_mtu.insert("haus2".into(), 1500);
        let d = derive(&config("heim"), &runtime);
        let has = |c: &str| d.mss.iter().any(|r| r.comment() == c);
        assert!(has("wl mss oif vx-buero ip 1306"));
        assert!(
            has("wl mss oif vx-haus2 ip 1306"),
            "min(measured 1318, 1418)"
        );
    }

    #[test]
    fn fixed_mss_overrides_measurements() {
        let network = NETWORK.replace("# mss = 1300", "mss = 1300");
        let local = "self = \"heim\"\ntrunk = \"ens19\"\n";
        let Loaded::Configured(cfg) = load(&network, local).unwrap().loaded else {
            panic!("configured");
        };
        let mut runtime = Runtime::default();
        runtime.pmtu.insert("buero".into(), 1400);
        let d = derive(&cfg, &runtime);
        assert_eq!(d.mss.len(), 12);
        assert!(
            d.mss
                .iter()
                .all(|r| r.mss == if r.ipv6 { 1280 } else { 1300 })
        );
    }

    #[test]
    fn blocked_tunnels_are_disabled() {
        let runtime = Runtime {
            blocked: BTreeSet::from(["vx-haus2".to_string()]),
            ..Runtime::default()
        };
        let d = derive(&config("heim"), &runtime);
        let enabled: Vec<_> = d
            .tunnels
            .iter()
            .map(|t| (t.name.as_str(), t.enabled))
            .collect();
        assert_eq!(
            enabled,
            vec![("vx-buero", true), ("vx-haus2", false), ("vx-haus3", true)]
        );
    }

    #[test]
    fn bridges_of_further_trunks() {
        assert_eq!(bridge_for("ens19", "ens19"), "br0");
        assert_eq!(bridge_for("ens20", "ens19"), "wl-ens20");
        assert_eq!(bridge_for("enp0s31f6", "ens19"), "wl-enp0s31f6");
        let long = bridge_for("enp0s31f6.1000", "ens19");
        assert!(long.starts_with("wl-") && long.len() <= 15, "{long}");
        assert_eq!(long, bridge_for("enp0s31f6.1000", "ens19"), "stable");
        assert_ne!(long, bridge_for("enp0s31f6.1001", "ens19"));
    }

    #[test]
    fn a_tunnel_with_its_own_trunk_gets_its_own_bridge() {
        let local = "self = \"heim\"\ntrunk = \"ens19\"\n\n[tunnels.haus2]\ntrunk = \"ens20\"\n[tunnels.haus3]\ntrunk = \"ens19\"\n";
        let cfg = match load(NETWORK, local).unwrap().loaded {
            Loaded::Configured(cfg) => cfg,
            Loaded::Unconfigured { .. } => panic!("configured"),
        };
        let d = derive(&cfg, &Runtime::default());
        assert_eq!(
            d.trunks
                .iter()
                .map(|t| (t.name.as_str(), t.bridge.as_str()))
                .collect::<Vec<_>>(),
            [("ens19", "br0"), ("ens20", "wl-ens20")]
        );
        assert_eq!(d.trunks[0].vlans, d.trunks[1].vlans);
        for t in &d.tunnels {
            let expected = if t.peer_name == "haus2" {
                "wl-ens20"
            } else {
                "br0"
            };
            assert_eq!(t.bridge, expected, "{}", t.peer);
        }
    }

    #[test]
    fn the_ports_of_clients_are_not_isolated() {
        let network = NETWORK.replace(
            "name = \"haus2\"\n",
            "name = \"haus2\"\nuplink = \"heim\"\n",
        );
        let cfg = match load(&network, "self = \"heim\"\ntrunk = \"ens19\"\n")
            .unwrap()
            .loaded
        {
            Loaded::Configured(cfg) => cfg,
            Loaded::Unconfigured { .. } => panic!("configured"),
        };
        let d = derive(&cfg, &Runtime::default());
        let haus2 = d.tunnels.iter().find(|t| t.peer_name == "haus2").unwrap();
        assert!(!haus2.isolated, "a client of heim");
        assert!(
            d.tunnels
                .iter()
                .filter(|t| t.peer_name != "haus2")
                .all(|t| t.isolated)
        );
        // haus2 has only its tunnel to heim.
        let cfg2 = match load(&network, "self = \"haus2\"\ntrunk = \"ens19\"\n")
            .unwrap()
            .loaded
        {
            Loaded::Configured(cfg) => cfg,
            Loaded::Unconfigured { .. } => panic!("configured"),
        };
        let d2 = derive(&cfg2, &Runtime::default());
        assert_eq!(
            d2.tunnels
                .iter()
                .map(|t| t.peer_name.as_str())
                .collect::<Vec<_>>(),
            ["heim"]
        );
    }

    #[test]
    fn own_vlans_go_to_the_trunk_and_down_but_never_up() {
        // heim: server; buero: client of heim (VLANs 20, own 60);
        // haus2: client of buero.
        let network = NETWORK
            .replace(
                "name = \"buero\"\naddress = \"10.8.0.2\"\n",
                "name = \"buero\"\naddress = \"10.8.0.2\"\nvlans = \"20\"\nown_vlans = \"60\"\nuplink = \"heim\"\n",
            )
            .replace(
                "name = \"haus2\"\naddress = \"10.8.0.11\"\n",
                "name = \"haus2\"\naddress = \"10.8.0.11\"\nuplink = \"buero\"\n",
            )
            .replace("vlans = \"20,30\"", "vlans = \"20,60\"");
        let derive_for = |me: &str| {
            let local = format!("self = \"{me}\"\ntrunk = \"ens19\"\n");
            match load(&network, &local).unwrap().loaded {
                Loaded::Configured(cfg) => derive(&cfg, &Runtime::default()),
                Loaded::Unconfigured { .. } => panic!("configured"),
            }
        };
        let vids = |d: &Desired, peer: &str| -> Vec<u16> {
            d.tunnels
                .iter()
                .find(|t| t.peer_name == peer)
                .unwrap()
                .vlans
                .keys()
                .copied()
                .collect()
        };
        let buero = derive_for("buero");
        assert_eq!(
            buero.trunk().vlans.keys().copied().collect::<Vec<_>>(),
            [20, 60],
            "own VLANs on the trunk"
        );
        assert_eq!(vids(&buero, "heim"), [20], "never up to the server");
        assert_eq!(vids(&buero, "haus2"), [20, 60], "down to its client");
        let heim = derive_for("heim");
        assert_eq!(vids(&heim, "buero"), [20], "the server agrees");
        let haus2 = derive_for("haus2");
        assert_eq!(vids(&haus2, "buero"), [20, 60], "the client agrees");
    }
}
