//! Local internet breakout: the public destinations of chosen VLANs leave
//! through the box's own uplink instead of the tunnel.
//!
//! The devices of such a VLAN notice nothing: their gateway, DHCP and DNS
//! stay those of the network (usually at the server). wanderlan only looks
//! at the destination: a frame that arrives at a trunk port of this site in
//! the VLAN and is addressed to a public IPv4 or IPv6 address goes into the
//! IP stack of the box instead of the tunnel, and leaves through the uplink
//! with the uplink's address (masquerade). Private destinations (RFC 1918,
//! CGNAT, link-local, ULA, multicast, ...) and the exceptions of the VLAN
//! go through the tunnel as before.
//!
//! Per breakout VLAN of a segment (bridge) there is a veth pair:
//! `wlbp<vid>` is a port of the bridge (only in that VLAN, untagged),
//! `wlbo<vid>` its other end in the IP stack ("the router"). The bridge
//! rewrites the destination MAC of the frames above to the one of the
//! router (nftables, table `bridge wanderlan-breakout`), so its own lookup
//! delivers them there. Their connections get a mark (table `inet
//! wanderlan-breakout`), which brings the answers back to the router by a
//! routing rule; the router finds the device by ARP or neighbour discovery
//! itself, so the box never needs to know the addresses of the VLAN.
//!
//! This module holds what the configuration, the desired state and the
//! views share: names, addresses, marks and which destinations are public.

use std::net::{IpAddr, Ipv4Addr};

pub use ipnet::IpNet;
use ipnet::{Ipv4Net, Ipv6Net};

/// Prefix of the router end of a breakout (in the IP stack).
pub const ROUTER_PREFIX: &str = "wlbo";
/// Prefix of the bridge port end of a breakout.
pub const PORT_PREFIX: &str = "wlbp";
/// Most exceptions per VLAN.
pub const MAX_EXCEPT: usize = 256;
/// Segments (bridges) that can have a breakout: the mark holds the segment
/// in 4 bits next to the VLAN ID.
pub const MAX_SEGMENTS: usize = 16;
/// The address of every router end (RFC 7600, the IPv4 dummy address): the
/// sender of its ARP requests. It is no address of any network, so the ARP
/// caches of the devices never learn a real address of the box.
pub const ROUTER_ADDRESS: Ipv4Addr = Ipv4Addr::new(192, 0, 0, 8);
/// Connection mark of the breakout connections: `MARK_BASE | id`.
pub const MARK_BASE: u32 = 0x5742_0000;
/// The bits of [`MARK_BASE`].
pub const MARK_MASK: u32 = 0xffff_0000;
/// Routing table of the way back to a router: `MARK_BASE | id`.
pub const BACK_TABLE_BASE: u32 = MARK_BASE;
/// Routing table of a fixed uplink of a router: `OUT_TABLE_BASE | id`.
pub const OUT_TABLE_BASE: u32 = 0x5743_0000;
/// Priority of the routing rules: before the main table and the rules of
/// the usual VPNs (Tailscale from 5210, wg-quick at 32764).
pub const RULE_PRIORITY: u32 = 1100;
/// Routing protocol of the rules and routes of wanderlan (`proto 87`): the
/// teardown finds them by it, without the configuration.
pub const RT_PROTOCOL: u8 = 87;

/// IPv4 destinations that are never public: they go through the tunnel.
pub const NOT_PUBLIC_V4: [&str; 11] = [
    "0.0.0.0/8",
    "10.0.0.0/8",
    "100.64.0.0/10",
    "127.0.0.0/8",
    "169.254.0.0/16",
    "172.16.0.0/12",
    "192.0.0.0/24",
    "192.168.0.0/16",
    "198.18.0.0/15",
    "224.0.0.0/4",
    "240.0.0.0/4",
];

/// IPv6 destinations that are never public, including the NAT64 prefixes
/// (an IPv4 address behind the NAT64 of the network is reached there).
pub const NOT_PUBLIC_V6: [&str; 8] = [
    "::/128",
    "::1/128",
    "::ffff:0:0/96",
    "64:ff9b::/96",
    "64:ff9b:1::/48",
    "fc00::/7",
    "fe80::/10",
    "ff00::/8",
];

/// The IPv4 networks of [`NOT_PUBLIC_V4`].
pub fn not_public_v4() -> Vec<Ipv4Net> {
    NOT_PUBLIC_V4
        .iter()
        .filter_map(|n| n.parse().ok())
        .collect()
}

/// The IPv6 networks of [`NOT_PUBLIC_V6`].
pub fn not_public_v6() -> Vec<Ipv6Net> {
    NOT_PUBLIC_V6
        .iter()
        .filter_map(|n| n.parse().ok())
        .collect()
}

/// True if `addr` is a public destination (not in the lists above).
pub fn is_public(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(a) => !not_public_v4().iter().any(|n| n.contains(&a)),
        IpAddr::V6(a) => !not_public_v6().iter().any(|n| n.contains(&a)),
    }
}

/// Reads an exception: a network (`203.0.113.0/24`, host bits are cleared)
/// or a single address (`203.0.113.7`, `2001:db8::1`).
pub fn parse_net(text: &str) -> Option<IpNet> {
    let text = text.trim();
    if text.contains('/') {
        text.parse::<IpNet>().ok().map(|n| n.trunc())
    } else {
        text.parse::<IpAddr>().ok().map(IpNet::from)
    }
}

/// The names of the breakout of VLAN `vid` in segment `segment` (0: the
/// default trunk): the router end and the bridge port.
pub fn names(segment: usize, vid: u16) -> (String, String) {
    if segment == 0 {
        (
            format!("{ROUTER_PREFIX}{vid}"),
            format!("{PORT_PREFIX}{vid}"),
        )
    } else {
        (
            format!("{ROUTER_PREFIX}{vid}s{segment}"),
            format!("{PORT_PREFIX}{vid}s{segment}"),
        )
    }
}

/// True for the names of breakout interfaces (either end).
pub fn is_breakout_name(name: &str) -> bool {
    [ROUTER_PREFIX, PORT_PREFIX].iter().any(|p| {
        name.strip_prefix(p)
            .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
    })
}

/// True for the bridge port end of a breakout.
pub fn is_port_name(name: &str) -> bool {
    is_breakout_name(name) && name.starts_with(PORT_PREFIX)
}

/// The MAC address of the router end: locally administered, unicast.
pub fn mac(segment: usize, vid: u16) -> [u8; 6] {
    let [hi, lo] = vid.to_be_bytes();
    // At most MAX_SEGMENTS segments, so it fits.
    #[allow(clippy::cast_possible_truncation)]
    let segment = segment as u8;
    [0x02, 0x77, 0x6c, segment, hi, lo]
}

/// The number of the breakout of VLAN `vid` in `segment`, in its mark and
/// tables; `None` for a segment beyond [`MAX_SEGMENTS`].
pub fn id(segment: usize, vid: u16) -> Option<u16> {
    let segment = u16::try_from(segment).ok()?;
    (usize::from(segment) < MAX_SEGMENTS).then(|| segment * 4096 + (vid & 0x0fff))
}

/// A MAC address as text.
pub fn mac_text(mac: &[u8; 6]) -> String {
    mac.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_and_private_destinations() {
        for public in ["203.0.113.1", "1.1.1.1", "2001:db8::1", "2606:4700::1111"] {
            assert!(is_public(public.parse().unwrap()), "{public}");
        }
        for private in [
            "10.1.2.3",
            "172.20.0.1",
            "192.168.1.1",
            "100.64.0.1",
            "169.254.1.1",
            "224.0.0.251",
            "255.255.255.255",
            "fd00::1",
            "fe80::1",
            "ff02::1",
            "64:ff9b::808:808",
        ] {
            assert!(!is_public(private.parse().unwrap()), "{private}");
        }
    }

    #[test]
    fn exceptions_are_networks_or_addresses() {
        assert_eq!(
            parse_net(" 203.0.113.7/24 ").unwrap().to_string(),
            "203.0.113.0/24"
        );
        assert_eq!(
            parse_net("203.0.113.7").unwrap().to_string(),
            "203.0.113.7/32"
        );
        assert_eq!(
            parse_net("2001:db8::1").unwrap().to_string(),
            "2001:db8::1/128"
        );
        assert!(parse_net("example.com").is_none());
        assert!(parse_net("10.0.0.0/33").is_none());
    }

    #[test]
    fn names_marks_and_macs() {
        assert_eq!(names(0, 10), ("wlbo10".into(), "wlbp10".into()));
        assert_eq!(names(2, 4094), ("wlbo4094s2".into(), "wlbp4094s2".into()));
        assert!(names(15, 4094).0.len() <= 15);
        assert!(is_breakout_name("wlbo10") && is_breakout_name("wlbp10s1"));
        assert!(!is_breakout_name("wlbond0") && !is_breakout_name("wl-ens20"));
        assert!(is_port_name("wlbp10") && !is_port_name("wlbo10"));
        assert_eq!(id(0, 10), Some(10));
        assert_eq!(id(1, 10), Some(4106));
        assert_eq!(id(16, 10), None);
        assert_eq!(mac_text(&mac(0, 10)), "02:77:6c:00:00:0a");
    }
}
