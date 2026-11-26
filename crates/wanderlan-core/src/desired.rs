//! Desired system state derived from the configuration.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{Ipv4Addr, SocketAddrV4};

use crate::BRIDGE_NAME;
use crate::config::{Config, LocalVid, NATIVE_VID, TunnelMode};
use crate::forwarding;
use crate::identity::PeerIdentity;
use crate::mtu;
use crate::sys::nft::{MssRule, PortDir, SplitRule};
use crate::sys::nft_ip::LearnRule;
use crate::sys::tc::{Direction, TcAction, TcRule};
use crate::sys::{BridgeOptions, PortVlans, VlanFlags, VxlanParams};
use crate::topology::{self, TunnelKind};

/// Alias that marks interfaces created by wanderlan.
pub const OWNER_ALIAS: &str = "wanderlan";
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
    /// Interface name (`vx-<peer>`).
    pub name: String,
    /// Remote site name.
    pub peer: String,
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
    /// Tunnels that cannot be built, by peer, with the reason.
    pub unbuilt: BTreeMap<String, String>,
    /// The clients whose endpoints are learned (table `ip wanderlan`).
    pub learn: BTreeSet<LearnRule>,
    /// The tunnel packets of the clients skip connection tracking (only
    /// where it is loaded).
    pub notrack: bool,
    /// The tunnels through NAT (by VNI): their packets get the tunnel port
    /// as source port (see [`crate::sys::nft_ip`]).
    pub fixed: BTreeSet<u32>,
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
}

/// The bridge of `trunk`: [`BRIDGE_NAME`] for the default trunk,
/// `wl-<trunk>` for every other (`wl-` and a hash if the name is too long).
pub fn bridge_for(trunk: &str, default_trunk: &str) -> String {
    if trunk == default_trunk {
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
    pub pmtu: BTreeMap<String, u16>,
    /// MTU of the interface the tunnel to a peer site leaves by: the path
    /// MTU is at most that, so it gives an MSS until the path is measured.
    pub underlay_mtu: BTreeMap<String, u16>,
    /// VNIs of the tunnels to peers whose identity is known (both sites
    /// have one, the keys derive from them); the others use a VNI from the
    /// two names and come up without proof of identity.
    pub vnis: BTreeMap<String, u32>,
    /// What the probes found about the identity of the peers with a key. A
    /// tunnel with a key comes up only once its peer is confirmed.
    pub identity: BTreeMap<String, PeerIdentity>,
    /// Where the tunnel packets of the clients of this site come from (the
    /// NAT in front of them chooses the port), learned from their
    /// keepalives.
    pub endpoints: BTreeMap<String, SocketAddrV4>,
    /// The clients of this site that it reaches directly, at this address
    /// (they passed the test of `wanderlan_core::nat`): their tunnels run
    /// direct.
    pub direct: BTreeMap<String, Ipv4Addr>,
    /// The uplink of this site (a client) reaches it directly and runs the
    /// tunnel direct.
    pub uplink_direct: bool,
    /// Connection tracking is loaded (it would cost every tunnel packet).
    pub conntrack: bool,
}

impl Runtime {
    /// How the tunnel `t` runs: servers among each other direct, a client
    /// tunnel direct once the server reached the client directly.
    pub fn mode(&self, t: &topology::Tunnel<'_>) -> TunnelMode {
        let direct = match t.kind {
            TunnelKind::Server => true,
            TunnelKind::Uplink => self.uplink_direct,
            TunnelKind::Client => self.direct.contains_key(&t.peer.name),
        };
        if direct {
            TunnelMode::Direct
        } else {
            TunnelMode::Nat
        }
    }
}

/// Computes the desired state.
pub fn derive(cfg: &Config, runtime: &Runtime) -> Desired {
    let blocked = &runtime.blocked;
    let network = &cfg.network;
    let me = cfg.self_name();
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
    let default_trunk = cfg.trunk();

    let self_online = cfg.self_site().is_none_or(|s| s.online);
    let tunnel_port = network.tunnel_port();
    let passing = forwarding::forwarding(network, me);
    let mut has_clients = false;
    let mut unbuilt = BTreeMap::new();
    let mut learn = BTreeSet::new();
    let mut vni_of: BTreeMap<u32, String> = BTreeMap::new();
    let mut tunnels: Vec<TunnelSpec> = Vec::new();
    for t in topology::tunnels_for(network, me) {
        let name = t.ifname();
        // What the tunnel carries: the VLANs both ends take part in (on
        // the way down also the own VLANs of the upper site), at most its
        // own limit.
        let carried = topology::tunnel_vlans(network, me, &t.peer.name).unwrap_or_default();
        let keyed = runtime.vnis.get(&t.peer.name).copied();
        let vni = keyed.unwrap_or_else(|| fallback_vni(me, &t.peer.name));
        // One VNI per UDP port on a box (the kernel refuses a second).
        if let Some(other) = vni_of.get(&vni) {
            unbuilt.insert(
                t.peer.name.clone(),
                format!(
                    "its tunnel would get the same VNI {vni} as the one to \"{other}\" (by chance, one in 16 million); a `wanderlan reset` and a new join of \"{}\" gives it another",
                    t.peer.name
                ),
            );
            continue;
        }
        vni_of.insert(vni, t.peer.name.clone());
        // Down while blocked by the loop guard, or while this site or the
        // peer is offline (set up but not taken online yet).
        let identity = runtime.identity.get(&t.peer.name).copied();
        let down_reason = if blocked.contains(&name) {
            Some("blocked by the loop guard".to_string())
        } else if !self_online {
            Some("this site is offline".to_string())
        } else if !t.peer.online {
            Some(format!("site \"{}\" is offline", t.peer.name))
        } else if keyed.is_some() {
            identity_reason(&t.peer.name, t.peer.address, identity.unwrap_or_default())
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
                learn.insert(LearnRule {
                    client: t.peer.name.clone(),
                    vni,
                });
                runtime.endpoints.get(&t.peer.name).copied()
            }
            (TunnelKind::Client, TunnelMode::Direct) => runtime
                .direct
                .get(&t.peer.name)
                .map(|ip| SocketAddrV4::new(*ip, tunnel_port)),
            _ => Some(SocketAddrV4::new(t.peer.address, tunnel_port)),
        };
        let down_reason = down_reason.or_else(|| {
            remote.is_none().then(|| {
                format!(
                    "waiting for client \"{}\" to connect (its tunnel packets, UDP {tunnel_port})",
                    t.peer.name
                )
            })
        });
        tunnels.push(TunnelSpec {
            enabled: down_reason.is_none(),
            down_reason,
            name,
            peer: t.peer.name.clone(),
            bridge: bridge_for(cfg.trunk_for(&t.peer.name), default_trunk),
            params: VxlanParams {
                vni,
                port: tunnel_port,
                source_port: (mode == TunnelMode::Nat).then_some(tunnel_port),
            },
            remote,
            isolated: passing.isolated.contains(&t.peer.name),
            vlans: carried.iter().map(|v| (v, VlanFlags::TAGGED)).collect(),
        });
    }

    // Every trunk port carries the same VLANs and mappings; the ports of the
    // default segment always exist, those of a tunnel's own segment while
    // the tunnel does. The first port of a list names its segment's bridge.
    let mut segments: Vec<&[String]> = vec![cfg.local.trunk.as_slice()];
    let mut extra: Vec<&[String]> = tunnels
        .iter()
        .map(|t| cfg.trunk_ports_for(&t.peer))
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
    let notrack = runtime.conntrack && has_clients;
    let fixed = tunnels
        .iter()
        .filter(|t| t.params.source_port.is_some())
        .map(|t| t.params.vni)
        .collect();
    Desired {
        tunnel_port,
        unbuilt,
        notrack,
        fixed,
        learn,
        bridge_options: BRIDGE_OPTIONS,
        trunks,
        mss: mss_rules(cfg, &tunnels, runtime),
        split: passing
            .split
            .iter()
            .map(|(peers, vlans)| SplitRule {
                ports: peers.iter().map(|p| topology::tunnel_name(p)).collect(),
                vlans: vlans.ranges(),
            })
            .collect(),
        tunnels,
        local_address,
    }
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
            .network
            .mss
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
    let me = cfg.self_name();
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
    for target in targets.iter().filter(|t| !locals.contains(t)) {
        rules.push(TcRule {
            direction: Direction::Ingress,
            vid: *target,
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
                .find(|t| t.peer == peer)
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
        let tunnel =
            |d: &Desired, peer: &str| d.tunnels.iter().find(|t| t.peer == peer).unwrap().clone();
        // "lan" passed the test: its server reached it at 10.8.0.32.
        let lan = Ipv4Addr::new(10, 8, 0, 32);
        let mut runtime = Runtime {
            direct: [("lan".to_string(), lan)].into(),
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
                .iter()
                .map(|r| (r.client.as_str(), r.vni))
                .collect::<Vec<_>>(),
            [("nat1", nat.params.vni)]
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
        assert!(client.learn.is_empty());
        assert_eq!(client.fixed, [up.params.vni].into());
        let reached = Runtime {
            uplink_direct: true,
            ..Runtime::default()
        };
        let lan_side = derive(&cfg("lan"), &reached);
        assert_eq!(tunnel(&lan_side, "heim").params.source_port, None);
        assert!(lan_side.fixed.is_empty());
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
                .map(|t| t.peer.as_str())
                .collect::<Vec<_>>(),
            ["buero", "haus3"]
        );
        assert!(d.unbuilt["haus2"].contains("same VNI 77 as the one to \"buero\""));
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
        assert_eq!(rules, vec!["wl:i30x", "wl:i130>30", "wl:e30>130"]);
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
            let expected = if t.peer == "haus2" { "wl-ens20" } else { "br0" };
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
        let haus2 = d.tunnels.iter().find(|t| t.peer == "haus2").unwrap();
        assert!(!haus2.isolated, "a client of heim");
        assert!(
            d.tunnels
                .iter()
                .filter(|t| t.peer != "haus2")
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
                .map(|t| t.peer.as_str())
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
                .find(|t| t.peer == peer)
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
