//! Desired system state derived from the configuration.

use std::collections::{BTreeMap, BTreeSet};
use std::net::Ipv4Addr;

use crate::BRIDGE_NAME;
use crate::config::{Config, LocalVid, NATIVE_VID};
use crate::mtu;
use crate::sys::nft::{MssRule, PortDir};
use crate::sys::tc::{Direction, TcAction, TcRule};
use crate::sys::{BridgeOptions, GretapParams, PortVlans, VlanFlags};
use crate::topology;

/// Alias that marks interfaces created by wanderlan.
pub const OWNER_ALIAS: &str = "wanderlan";
/// MTU of the GRETAP interfaces: full Ethernet frames; GRE fragments the
/// encapsulated packets if the transport path is smaller.
pub const GRETAP_MTU: u32 = 1500;

/// Options wanderlan sets on its bridge.
pub const BRIDGE_OPTIONS: BridgeOptions = BridgeOptions {
    vlan_filtering: true,
    stp: false,
    default_pvid: 0,
    nf_call: false,
};

/// The trunk port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrunkSpec {
    /// Interface name.
    pub name: String,
    /// VLAN membership.
    pub vlans: PortVlans,
    /// VLAN translation rules (tc), sorted.
    pub tc: Vec<TcRule>,
}

/// One GRETAP tunnel port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelSpec {
    /// Interface name (`gt-<peer>`).
    pub name: String,
    /// Remote site name.
    pub peer: String,
    /// GRETAP parameters.
    pub params: GretapParams,
    /// Split horizon: isolated ports do not forward to each other. Every
    /// tunnel port is isolated, so frames from one tunnel never enter another.
    pub isolated: bool,
    /// Whether the port should be up (false if blocked by the loop guard).
    pub enabled: bool,
    /// VLAN membership: the VLANs both ends take part in.
    pub vlans: PortVlans,
}

/// Everything wanderlan manages on one site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Desired {
    /// Bridge name.
    pub bridge: String,
    /// Bridge options.
    pub bridge_options: BridgeOptions,
    /// The local trunk.
    pub trunk: TrunkSpec,
    /// Tunnels to the peer sites, sorted by name.
    pub tunnels: Vec<TunnelSpec>,
    /// Local address of the site (GRE source).
    pub local_address: Ipv4Addr,
    /// TCP MSS clamping rules (nftables).
    pub mss: BTreeSet<MssRule>,
}

/// Runtime information that, besides the configuration, shapes the desired
/// state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Runtime {
    /// Tunnel interfaces shut down by the loop guard.
    pub blocked: BTreeSet<String>,
    /// Measured path MTU per peer site.
    pub pmtu: BTreeMap<String, u16>,
}

/// Computes the desired state.
pub fn derive(cfg: &Config, runtime: &Runtime) -> Desired {
    let blocked = &runtime.blocked;
    let mesh = &cfg.mesh;
    let me = cfg.self_name();
    let local_address = cfg.self_site().map_or(Ipv4Addr::UNSPECIFIED, |s| s.address);

    let own = cfg
        .self_site()
        .map(|s| mesh.effective_vlans(s))
        .unwrap_or_default();
    // The native VLAN leaves the trunk untagged (and untagged frames belong
    // to it); on the tunnels every VLAN is tagged.
    let shared: PortVlans = own
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

    let self_online = cfg.self_site().is_none_or(|s| s.online);
    let tunnels: Vec<TunnelSpec> = topology::tunnels_for(mesh, me)
        .into_iter()
        .map(|t| {
            let name = t.ifname();
            let theirs = mesh.effective_vlans(t.peer);
            TunnelSpec {
                // Down while blocked by the loop guard, or while this site or
                // the peer is offline (set up but not taken online yet).
                enabled: !blocked.contains(&name) && self_online && t.peer.online,
                name,
                peer: t.peer.name.clone(),
                params: GretapParams {
                    local: local_address,
                    remote: t.peer.address,
                    ignore_df: true,
                    pmtudisc: false,
                },
                isolated: true,
                vlans: own
                    .iter()
                    .filter(|v| theirs.contains(*v))
                    .map(|v| (v, VlanFlags::TAGGED))
                    .collect(),
            }
        })
        .collect();

    Desired {
        bridge: BRIDGE_NAME.to_string(),
        bridge_options: BRIDGE_OPTIONS,
        trunk: TrunkSpec {
            name: cfg.trunk().to_string(),
            vlans: trunk_vlans,
            tc,
        },
        mss: mss_rules(cfg, &tunnels, runtime),
        tunnels,
        local_address,
    }
}

/// MSS clamping per tunnel port: a fixed value from the configuration, or
/// the value derived from the measured path MTU of the peer. Ports without a
/// measurement use the smallest measured value; without any measurement
/// there are no rules yet. Rules apply to SYNs entering and leaving through
/// the port, so both ends of a connection see the smaller value.
fn mss_rules(cfg: &Config, tunnels: &[TunnelSpec], runtime: &Runtime) -> BTreeSet<MssRule> {
    let measured: BTreeMap<&str, u16> = tunnels
        .iter()
        .filter_map(|t| {
            runtime
                .pmtu
                .get(&t.peer)
                .map(|p| (t.name.as_str(), mtu::mss_for_pmtu(*p)))
        })
        .collect();
    let fallback = measured.values().copied().min();
    let mut out = BTreeSet::new();
    for t in tunnels {
        let mss = cfg
            .mesh
            .mss
            .or_else(|| measured.get(t.name.as_str()).copied())
            .or(fallback);
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
    for map in cfg.mesh.maps_of(me) {
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
mod tests {
    use super::*;
    use crate::config::{Loaded, load};

    const MESH: &str = include_str!("../../../packaging/examples/mesh.toml");

    fn config(self_name: &str) -> Config {
        let local = format!("self = \"{self_name}\"\ntrunk = \"ens19\"\n");
        match load(MESH, &local).unwrap().loaded {
            Loaded::Configured(cfg) => cfg,
            Loaded::Unconfigured { .. } => panic!("configured"),
        }
    }

    #[test]
    fn every_site_gets_an_isolated_tunnel_to_every_other_site() {
        let d = derive(&config("heim"), &Runtime::default());
        assert_eq!(d.bridge, "br0");
        assert_eq!(d.local_address, Ipv4Addr::new(10, 8, 0, 1));
        let summary: Vec<_> = d
            .tunnels
            .iter()
            .map(|t| (t.name.as_str(), t.isolated, t.params.remote))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("gt-buero", true, Ipv4Addr::new(10, 8, 0, 2)),
                ("gt-haus2", true, Ipv4Addr::new(10, 8, 0, 11)),
                ("gt-haus3", true, Ipv4Addr::new(10, 8, 0, 12)),
            ]
        );
        for t in &d.tunnels {
            assert!(t.params.ignore_df && !t.params.pmtudisc);
            assert_eq!(t.params.local, d.local_address);
        }
        assert_eq!(d.trunk.vlans.len(), 41);
        assert!(d.trunk.vlans.values().all(|f| *f == VlanFlags::TAGGED));
        assert!(!d.trunk.vlans.contains_key(&1));
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
        assert_eq!(vids(&heim, "gt-buero").len(), 41);
        assert_eq!(vids(&heim, "gt-haus2"), vec![20, 30]);
        let haus2 = derive(&config("haus2"), &Runtime::default());
        assert_eq!(
            haus2.trunk.vlans.keys().copied().collect::<Vec<_>>(),
            vec![20, 30]
        );
        assert_eq!(vids(&haus2, "gt-heim"), vec![20, 30]);
    }

    #[test]
    fn offline_sites_have_their_tunnels_down() {
        let mesh = MESH.replace(
            "name = \"buero\"\naddress = \"10.8.0.2\"\n",
            "name = \"buero\"\naddress = \"10.8.0.2\"\nonline = false\n",
        );
        let at = |site: &str| {
            let local = format!("self = \"{site}\"\ntrunk = \"ens19\"\n");
            let Loaded::Configured(cfg) = load(&mesh, &local).unwrap().loaded else {
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
                ("gt-buero".to_string(), false),
                ("gt-haus2".to_string(), true),
                ("gt-haus3".to_string(), true),
            ]
        );
        assert!(at("buero").iter().all(|(_, enabled)| !enabled));
    }

    #[test]
    fn native_vlan_is_untagged_on_the_trunk_and_tagged_in_tunnels() {
        // All VLANs: VLAN 1 (native) is untagged on the trunk; haus3 maps its
        // untagged traffic to VLAN 20, so the native VLAN does not reach it.
        let mesh = MESH.replace("vlans = \"10-50\"", "vlans = \"all\"");
        let local = "self = \"heim\"\ntrunk = \"ens19\"\n";
        let Loaded::Configured(cfg) = load(&mesh, local).unwrap().loaded else {
            panic!("configured");
        };
        let d = derive(&cfg, &Runtime::default());
        assert_eq!(d.trunk.vlans.get(&1), Some(&VlanFlags::NATIVE));
        assert_eq!(d.trunk.vlans.get(&4094), Some(&VlanFlags::TAGGED));
        assert_eq!(d.trunk.vlans.len(), 4094);
        let tunnel = |name: &str| {
            d.tunnels
                .iter()
                .find(|t| t.name == name)
                .map(|t| t.vlans.clone())
                .unwrap_or_default()
        };
        assert_eq!(tunnel("gt-buero").get(&1), Some(&VlanFlags::TAGGED));
        assert_eq!(tunnel("gt-buero").len(), 4094);
        assert_eq!(tunnel("gt-haus3").get(&1), None, "haus3 maps native to 20");
        assert_eq!(tunnel("gt-haus3").len(), 4093);

        let local = "self = \"haus3\"\ntrunk = \"ens19\"\n";
        let Loaded::Configured(cfg) = load(&mesh, local).unwrap().loaded else {
            panic!("configured");
        };
        let d = derive(&cfg, &Runtime::default());
        assert_eq!(d.trunk.vlans.get(&1), None);
        assert_eq!(d.trunk.vlans.get(&20), Some(&VlanFlags::NATIVE));
    }

    #[test]
    fn native_mapping_and_translation_shape_the_trunk() {
        let d = derive(&config("haus3"), &Runtime::default());
        assert_eq!(d.trunk.vlans.get(&20), Some(&VlanFlags::NATIVE));
        assert_eq!(d.trunk.vlans.get(&30), Some(&VlanFlags::TAGGED));
        assert!(!d.trunk.vlans.contains_key(&130));
        assert_eq!(d.tunnels.len(), 3);
        assert!(d.tunnels.iter().all(|t| t.isolated));
    }

    #[test]
    fn translation_rules_for_haus3() {
        let d = derive(&config("haus3"), &Runtime::default());
        let rules: Vec<String> = d.trunk.tc.iter().map(TcRule::cookie).collect();
        assert_eq!(rules, vec!["wl:i30x", "wl:i130>30", "wl:e30>130"]);
        assert!(
            derive(&config("heim"), &Runtime::default())
                .trunk
                .tc
                .is_empty()
        );
    }

    #[test]
    fn chained_translations_keep_targets_on_the_trunk() {
        // 130 -> 30 and 30 -> 40 at the same site: local 30 is translated, but
        // shared 30 is still needed as target of local 130.
        let mesh = MESH.replace("local = \"native\"\nshared = 20", "local = 30\nshared = 40");
        let local = "self = \"haus3\"\ntrunk = \"ens19\"\n";
        let Loaded::Configured(cfg) = load(&mesh, local).unwrap().loaded else {
            panic!("configured");
        };
        let d = derive(&cfg, &Runtime::default());
        assert_eq!(d.trunk.vlans.get(&30), Some(&VlanFlags::TAGGED));
        assert_eq!(d.trunk.vlans.get(&40), Some(&VlanFlags::TAGGED));
        let rules: Vec<String> = d.trunk.tc.iter().map(TcRule::cookie).collect();
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
        let mesh = MESH.replace("local = 130", "local = 15");
        let local = "self = \"haus3\"\ntrunk = \"ens19\"\n";
        let Loaded::Configured(cfg) = load(&mesh, local).unwrap().loaded else {
            panic!("configured");
        };
        let d = derive(&cfg, &Runtime::default());
        assert!(!d.trunk.vlans.contains_key(&15));
        assert_eq!(d.trunk.vlans.get(&30), Some(&VlanFlags::TAGGED));
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
                "wl mss iif gt-buero ip 1318",
                "wl mss iif gt-buero ip6 1298",
                "wl mss oif gt-buero ip 1318",
                "wl mss oif gt-buero ip6 1298",
                "wl mss iif gt-haus2 ip 1318",
                "wl mss iif gt-haus2 ip6 1298",
                "wl mss oif gt-haus2 ip 1318",
                "wl mss oif gt-haus2 ip6 1298",
                "wl mss iif gt-haus3 ip 1318",
                "wl mss iif gt-haus3 ip6 1298",
                "wl mss oif gt-haus3 ip 1318",
                "wl mss oif gt-haus3 ip6 1298",
            ]
        );
        runtime.pmtu.insert("haus2".into(), 1500);
        let d = derive(&config("heim"), &runtime);
        assert!(
            d.mss
                .iter()
                .any(|r| r.comment() == "wl mss oif gt-haus2 ip 1418")
        );
        assert!(
            d.mss
                .iter()
                .any(|r| r.comment() == "wl mss oif gt-buero ip 1318")
        );
    }

    #[test]
    fn fixed_mss_overrides_measurements() {
        let mesh = MESH.replace("# mss = 1300", "mss = 1300");
        let local = "self = \"heim\"\ntrunk = \"ens19\"\n";
        let Loaded::Configured(cfg) = load(&mesh, local).unwrap().loaded else {
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
            blocked: BTreeSet::from(["gt-haus2".to_string()]),
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
            vec![("gt-buero", true), ("gt-haus2", false), ("gt-haus3", true)]
        );
    }
}
