//! The traffic per VLAN as the daemon reports it (`vlan_traffic`), in the
//! shapes the peer details and the VLAN window need.

use wanderlan_core::api::{VlanPortTraffic, VlanTraffic};

/// The VLANs on the tunnel to `peer`, the busiest first (then by VLAN).
pub fn peer_vlans<'a>(traffic: &'a VlanTraffic, peer: &str) -> Vec<&'a VlanPortTraffic> {
    let mut list: Vec<&VlanPortTraffic> = traffic
        .ports
        .iter()
        .filter(|p| p.peer.as_deref() == Some(peer))
        .collect();
    list.sort_by(|a, b| {
        (b.rx_bps + b.tx_bps)
            .cmp(&(a.rx_bps + a.tx_bps))
            .then((b.rx_bytes + b.tx_bytes).cmp(&(a.rx_bytes + a.tx_bytes)))
            .then(a.vid.cmp(&b.vid))
    });
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

/// The whole traffic of one VLAN: into the bridge (sum of what all ports
/// received) and out of it (sum of what they sent; flooded frames count on
/// every port), in bit/s.
pub fn vlan_total(traffic: &VlanTraffic, vid: u16) -> (u64, u64) {
    traffic
        .ports
        .iter()
        .filter(|p| p.vid == vid)
        .fold((0, 0), |(rx, tx), p| (rx + p.rx_bps, tx + p.tx_bps))
}

/// The VLAN after or before `vid` (`step` +1 or -1), staying at the ends.
pub fn next_vlan(traffic: &VlanTraffic, vid: u16, step: isize) -> u16 {
    let ids = vlan_ids(traffic);
    let at = ids
        .iter()
        .position(|v| *v >= vid)
        .unwrap_or(ids.len().saturating_sub(1));
    let at = if ids.get(at) == Some(&vid) || step < 0 {
        at.saturating_add_signed(step)
    } else {
        at
    };
    ids.get(at.min(ids.len().saturating_sub(1)))
        .copied()
        .unwrap_or(vid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn port(port: &str, vid: u16, rx_bps: u64) -> VlanPortTraffic {
        VlanPortTraffic {
            port: port.into(),
            peer: port.strip_prefix("gt-").map(str::to_string),
            vid,
            rx_bps,
            tx_bps: rx_bps / 2,
            ..VlanPortTraffic::default()
        }
    }

    fn traffic() -> VlanTraffic {
        VlanTraffic {
            available: true,
            note: None,
            ports: vec![
                port("ens19", 20, 900),
                port("gt-buero", 10, 100),
                port("gt-buero", 20, 5000),
                port("gt-buero", 30, 0),
                port("gt-haus2", 20, 50),
            ],
        }
    }

    #[test]
    fn the_busiest_vlans_of_a_peer_first() {
        let t = traffic();
        let vids: Vec<u16> = peer_vlans(&t, "buero").iter().map(|p| p.vid).collect();
        assert_eq!(vids, [20, 10, 30]);
        assert!(peer_vlans(&t, "haus9").is_empty());
    }

    #[test]
    fn one_vlan_on_all_ports() {
        let t = traffic();
        let ports: Vec<&str> = vlan_ports(&t, 20).iter().map(|p| p.port.as_str()).collect();
        assert_eq!(ports, ["gt-buero", "gt-haus2", "ens19"], "the trunk last");
        assert_eq!(vlan_total(&t, 20), (5950, 2975));
        assert_eq!(vlan_ids(&t), [10, 20, 30]);
        assert_eq!(next_vlan(&t, 20, 1), 30);
        assert_eq!(next_vlan(&t, 20, -1), 10);
        assert_eq!(next_vlan(&t, 30, 1), 30, "stays at the end");
        assert_eq!(next_vlan(&t, 10, -1), 10);
        assert_eq!(next_vlan(&t, 15, 1), 20, "a VLAN not on a port");
        assert_eq!(next_vlan(&t, 15, -1), 10);
    }
}
