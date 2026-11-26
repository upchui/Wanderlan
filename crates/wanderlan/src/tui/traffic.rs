//! The traffic per VLAN as the daemon reports it (`vlan_traffic`), in the
//! shapes the peer details and the VLAN window need.

use wanderlan_core::api::{VlanPortTraffic, VlanTraffic};

/// The VLANs on the tunnel to `peer`: those that carried traffic first,
/// each part by VLAN. The order stays while the rates change (the cursor
/// moves through it).
pub fn peer_vlans<'a>(traffic: &'a VlanTraffic, peer: &str) -> Vec<&'a VlanPortTraffic> {
    let mut list: Vec<&VlanPortTraffic> = traffic
        .ports
        .iter()
        .filter(|p| p.peer.as_deref() == Some(peer))
        .collect();
    let unused = |p: &VlanPortTraffic| p.rx_bytes == 0 && p.tx_bytes == 0;
    list.sort_by_key(|p| (unused(p), p.vid));
    list
}

/// Every VLAN on a port of this site, ascending.
pub fn vlan_ids(traffic: &VlanTraffic) -> Vec<u16> {
    let mut ids: Vec<u16> = traffic.ports.iter().map(|p| p.vid).collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// The ports of one VLAN: the tunnels by peer, then the trunk ports.
pub fn vlan_ports(traffic: &VlanTraffic, vid: u16) -> Vec<&VlanPortTraffic> {
    let mut list: Vec<&VlanPortTraffic> = traffic.ports.iter().filter(|p| p.vid == vid).collect();
    list.sort_by(|a, b| {
        a.peer
            .is_none()
            .cmp(&b.peer.is_none())
            .then(a.port.cmp(&b.port))
    });
    list
}

/// The whole traffic per VLAN: into the bridge (sum of what all ports
/// received) and out of it (sum of what they sent; flooded frames count on
/// every port), in bit/s. One pass over the entries (every VLAN on every
/// port: many with the default of all VLANs).
pub fn vlan_totals(traffic: &VlanTraffic) -> std::collections::BTreeMap<u16, (u64, u64)> {
    let mut totals = std::collections::BTreeMap::new();
    for p in &traffic.ports {
        let t: &mut (u64, u64) = totals.entry(p.vid).or_default();
        t.0 += p.rx_bps;
        t.1 += p.tx_bps;
    }
    totals
}

/// The VLAN after or before `vid` (`step` +1 or -1), staying at the ends.
pub fn next_vlan(traffic: &VlanTraffic, vid: u16, step: isize) -> u16 {
    let ids = vlan_ids(traffic);
    let pick = match (ids.binary_search(&vid), step >= 0) {
        // On the list: its neighbour.
        (Ok(at), true) => ids.get(at + 1),
        (Ok(at), false) => at.checked_sub(1).and_then(|i| ids.get(i)),
        // Not (any more) on the list: the next one after or before it.
        (Err(at), true) => ids.get(at),
        (Err(at), false) => at.checked_sub(1).and_then(|i| ids.get(i)),
    };
    pick.copied().unwrap_or(vid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn port(port: &str, vid: u16, rx_bps: u64) -> VlanPortTraffic {
        VlanPortTraffic {
            port: port.into(),
            peer: port.strip_prefix("vx-").map(str::to_string),
            vid,
            rx_bps,
            tx_bps: rx_bps / 2,
            rx_bytes: rx_bps,
            ..VlanPortTraffic::default()
        }
    }

    fn traffic() -> VlanTraffic {
        VlanTraffic {
            available: true,
            note: None,
            ports: vec![
                port("ens19", 20, 900),
                port("vx-buero", 10, 100),
                port("vx-buero", 20, 5000),
                port("vx-buero", 30, 0),
                port("vx-haus2", 20, 50),
            ],
        }
    }

    #[test]
    fn the_vlans_of_a_peer_that_carried_traffic_first() {
        let mut t = traffic();
        t.ports.push(port("vx-buero", 5, 0));
        let vids: Vec<u16> = peer_vlans(&t, "buero").iter().map(|p| p.vid).collect();
        assert_eq!(vids, [10, 20, 5, 30], "by VLAN, the unused ones last");
        assert!(peer_vlans(&t, "haus9").is_empty());
    }

    #[test]
    fn one_vlan_on_all_ports() {
        let t = traffic();
        let ports: Vec<&str> = vlan_ports(&t, 20).iter().map(|p| p.port.as_str()).collect();
        assert_eq!(ports, ["vx-buero", "vx-haus2", "ens19"], "the trunk last");
        assert_eq!(vlan_totals(&t)[&20], (5950, 2975));
        assert_eq!(vlan_ids(&t), [10, 20, 30]);
        assert_eq!(next_vlan(&t, 20, 1), 30);
        assert_eq!(next_vlan(&t, 20, -1), 10);
        assert_eq!(next_vlan(&t, 30, 1), 30, "stays at the end");
        assert_eq!(next_vlan(&t, 10, -1), 10);
        assert_eq!(next_vlan(&t, 15, 1), 20, "a VLAN not on a port");
        assert_eq!(next_vlan(&t, 15, -1), 10);
        // Gone at the end: Up is the last one, Down stays.
        assert_eq!(next_vlan(&t, 40, -1), 30);
        assert_eq!(next_vlan(&t, 40, 1), 40);
    }
}
