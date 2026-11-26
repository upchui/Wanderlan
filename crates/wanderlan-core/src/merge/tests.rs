use std::collections::{BTreeMap, BTreeSet};
use std::net::{Ipv4Addr, SocketAddrV4};

use super::*;
use crate::desired::{BRIDGE_OPTIONS, TunnelSpec};
use crate::identity::SiteId;
use crate::sys::nft_ip::{LearnRule, LearnTable};
use crate::sys::tc::{Direction, TcAction, TcRule};
use crate::sys::{VlanFlags, VxlanParams};

/// A network with one trunk port in `bridge` and a tunnel per peer, all
/// with `vlans`.
fn network(port: u16, trunk: &str, bridge: &str, vlans: &[u16], peers: &[&str]) -> Desired {
    let vlans: crate::sys::PortVlans = vlans.iter().map(|v| (*v, VlanFlags::TAGGED)).collect();
    let tunnels: Vec<TunnelSpec> = peers
        .iter()
        .enumerate()
        .map(|(i, peer)| TunnelSpec {
            name: format!("vx{peer}"),
            peer: SiteId::from(*peer),
            peer_name: (*peer).to_string(),
            alias: format!("wanderlan {peer}"),
            bridge: bridge.to_string(),
            params: VxlanParams {
                vni: 100 + i as u32,
                port,
                source_port: None,
            },
            remote: Some(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 2), port)),
            isolated: true,
            enabled: true,
            down_reason: None,
            vlans: vlans.clone(),
        })
        .collect();
    Desired {
        bridge_options: BRIDGE_OPTIONS,
        trunks: vec![TrunkSpec {
            name: trunk.to_string(),
            bridge: bridge.to_string(),
            vlans,
            tc: Vec::new(),
            required: true,
            down_reason: None,
        }],
        learn: LearnTable {
            rules: tunnels
                .iter()
                .map(|t| LearnRule {
                    client: t.name.clone(),
                    vni: t.params.vni,
                    port,
                })
                .collect(),
            ..LearnTable::default()
        },
        tunnels,
        local_address: Ipv4Addr::new(10, 0, 0, 1),
        mss: BTreeSet::new(),
        split: BTreeSet::new(),
        tunnel_port: port,
        unbuilt: BTreeMap::new(),
        breakout: Vec::new(),
    }
}

#[test]
fn one_network_is_the_box() {
    let home = network(51822, "trunk0", "br0", &[10, 20], &["a", "b"]);
    let merged = merge(&[("default", &home)]).unwrap();
    assert_eq!(merged.desired, home);
    assert!(merged.conflicts.is_empty());
    assert!(merge(&[]).is_none());
}

#[test]
fn networks_add_up_and_the_later_one_gives_way() {
    let home = network(51822, "trunk0", "br0", &[10, 20], &["a"]);
    // Its own port and bridge: everything stays, the VLANs may overlap.
    let iso = network(51830, "trunk1", "wl-trunk1", &[20], &["xa"]);
    // The trunk of the default network: into br0, without VLAN 20.
    let lab = network(51840, "trunk0", "wl-trunk0", &[20, 60], &["la"]);
    let merged = merge(&[("default", &home), ("iso", &iso), ("lab", &lab)]).unwrap();
    let box_ = &merged.desired;
    let names: Vec<&str> = box_.tunnels.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["vxa", "vxla", "vxxa"]);
    let bridges: Vec<&str> = box_.bridges().collect();
    assert_eq!(bridges, ["br0", "wl-trunk1"]);
    let la = box_.tunnels.iter().find(|t| t.name == "vxla").unwrap();
    assert_eq!(la.bridge, "br0");
    assert_eq!(la.vlans.keys().copied().collect::<Vec<_>>(), [60]);
    let trunk0 = box_.trunks.iter().find(|t| t.name == "trunk0").unwrap();
    assert_eq!(
        trunk0.vlans.keys().copied().collect::<Vec<_>>(),
        [10, 20, 60]
    );
    assert_eq!(box_.learn.gates(), [51822, 51830, 51840].into());
    assert!(!merged.conflicts.contains_key("iso"));
    let lost = &merged.conflicts["lab"];
    assert!(lost[0].contains("VLANs 20 in bridge br0"), "{lost:?}");

    // The same tunnel port: no tunnels of the later network.
    let twin = network(51822, "trunk2", "wl-trunk2", &[30], &["ta"]);
    let merged = merge(&[("default", &home), ("twin", &twin)]).unwrap();
    assert_eq!(merged.desired, home);
    assert!(merged.conflicts["twin"][0].contains("UDP 51822"));

    // A port with VLAN mappings is not shared.
    let mut mapped = home.clone();
    mapped.trunks[0].tc = vec![TcRule {
        direction: Direction::Ingress,
        vid: 30,
        action: TcAction::Translate(130),
    }];
    let merged = merge(&[("default", &mapped), ("lab", &lab)]).unwrap();
    assert_eq!(merged.desired, mapped, "nothing of lab is left");
    let lost = &merged.conflicts["lab"];
    assert!(lost[0].contains("maps VLANs"), "{lost:?}");
    assert!(lost[1].contains("tunnels vxla"), "{lost:?}");
}
