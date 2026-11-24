//! Desired system state derived from the configuration.

use std::collections::BTreeSet;
use std::net::Ipv4Addr;

use crate::BRIDGE_NAME;
use crate::config::{Config, LocalVid};
use crate::sys::tc::{Direction, TcAction, TcRule};
use crate::sys::{BridgeOptions, GretapParams, PortVlans, VlanFlags};
use crate::topology::{self, TunnelKind};

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
    /// Kind of the tunnel.
    pub kind: TunnelKind,
    /// GRETAP parameters.
    pub params: GretapParams,
    /// Split horizon: isolated ports do not forward to each other.
    pub isolated: bool,
    /// Whether the port should be up (false if blocked by the loop guard).
    pub enabled: bool,
    /// VLAN membership.
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
}

/// Computes the desired state. `blocked` lists tunnel interfaces the loop
/// guard has shut down.
pub fn derive(cfg: &Config, blocked: &BTreeSet<String>) -> Desired {
    let mesh = &cfg.mesh;
    let me = cfg.self_name();
    let local_address = cfg.self_site().map_or(Ipv4Addr::UNSPECIFIED, |s| s.address);

    let shared: PortVlans = mesh.vlans.iter().map(|v| (v, VlanFlags::TAGGED)).collect();
    let (trunk_vlans, tc) = trunk_mapping(cfg, &shared);

    let tunnels = topology::tunnels_for(mesh, me)
        .into_iter()
        .map(|t| {
            let name = t.ifname();
            TunnelSpec {
                enabled: !blocked.contains(&name),
                name,
                peer: t.peer.name.clone(),
                kind: t.kind,
                params: GretapParams {
                    local: local_address,
                    remote: t.peer.address,
                    ignore_df: true,
                    pmtudisc: false,
                },
                isolated: t.isolated(),
                vlans: shared.clone(),
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
        tunnels,
        local_address,
    }
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
    fn hub_has_isolated_hub_ports_and_open_spoke_ports() {
        let d = derive(&config("heim"), &BTreeSet::new());
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
                ("gt-haus2", false, Ipv4Addr::new(10, 8, 0, 11)),
            ]
        );
        for t in &d.tunnels {
            assert_eq!(t.vlans.len(), 41);
            assert!(t.params.ignore_df && !t.params.pmtudisc);
            assert_eq!(t.params.local, d.local_address);
        }
        assert_eq!(d.trunk.vlans.len(), 41);
        assert!(d.trunk.vlans.values().all(|f| *f == VlanFlags::TAGGED));
        assert!(!d.trunk.vlans.contains_key(&1));
    }

    #[test]
    fn native_mapping_and_translation_shape_the_trunk() {
        let d = derive(&config("haus3"), &BTreeSet::new());
        assert_eq!(d.trunk.vlans.get(&20), Some(&VlanFlags::NATIVE));
        assert_eq!(d.trunk.vlans.get(&30), Some(&VlanFlags::TAGGED));
        assert!(!d.trunk.vlans.contains_key(&130));
        assert_eq!(d.tunnels.len(), 1);
        assert_eq!(d.tunnels[0].name, "gt-buero");
        assert!(!d.tunnels[0].isolated);
    }

    #[test]
    fn translation_rules_for_haus3() {
        let d = derive(&config("haus3"), &BTreeSet::new());
        let rules: Vec<String> = d.trunk.tc.iter().map(TcRule::cookie).collect();
        assert_eq!(rules, vec!["wl:i30x", "wl:i130>30", "wl:e30>130"]);
        assert!(
            derive(&config("heim"), &BTreeSet::new())
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
        let d = derive(&cfg, &BTreeSet::new());
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
        let d = derive(&cfg, &BTreeSet::new());
        assert!(!d.trunk.vlans.contains_key(&15));
        assert_eq!(d.trunk.vlans.get(&30), Some(&VlanFlags::TAGGED));
    }

    #[test]
    fn blocked_tunnels_are_disabled() {
        let blocked = BTreeSet::from(["gt-haus2".to_string()]);
        let d = derive(&config("heim"), &blocked);
        let enabled: Vec<_> = d
            .tunnels
            .iter()
            .map(|t| (t.name.as_str(), t.enabled))
            .collect();
        assert_eq!(enabled, vec![("gt-buero", true), ("gt-haus2", false)]);
    }
}
