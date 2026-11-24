//! Desired system state derived from the configuration.

use std::collections::BTreeSet;
use std::net::Ipv4Addr;

use crate::BRIDGE_NAME;
use crate::config::{Config, LocalVid};
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

    // The trunk carries all shared VLANs, adjusted by the local mappings:
    // a native mapping makes the shared VLAN the untagged VLAN of the trunk;
    // a translated local VLAN is no longer available as shared VLAN here.
    let mut trunk_vlans = shared.clone();
    for map in mesh.maps_of(me) {
        match map.local {
            LocalVid::Native => {
                trunk_vlans.insert(map.shared, VlanFlags::NATIVE);
            }
            LocalVid::Vid(local) => {
                trunk_vlans.remove(&local);
            }
        }
    }

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
        },
        tunnels,
        local_address,
    }
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
