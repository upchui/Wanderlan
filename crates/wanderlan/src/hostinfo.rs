//! Facts about this host for the setup: host name, which interface leads to
//! an address, the source address towards it, and which interface is the
//! trunk. Read from /proc and the socket API; no privileges needed.

use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};

use wanderlan_core::api::Interface;

/// The host name (`/proc/sys/kernel/hostname`), empty if unknown.
pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// One IPv4 route of `/proc/net/route`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Route {
    iface: String,
    destination: Ipv4Addr,
    mask: Ipv4Addr,
    metric: u32,
}

/// Parses `/proc/net/route` (addresses as native-endian hex of the network
/// byte order value); only routes that are up.
fn parse_routes(text: &str) -> Vec<Route> {
    const RTF_UP: u32 = 0x1;
    let addr = |hex: &str| {
        u32::from_str_radix(hex, 16)
            .ok()
            .map(|v| Ipv4Addr::from(v.to_ne_bytes()))
    };
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            let flags = u32::from_str_radix(f.get(3)?, 16).ok()?;
            (flags & RTF_UP != 0).then_some(())?;
            Some(Route {
                iface: (*f.first()?).to_string(),
                destination: addr(f.get(1)?)?,
                mask: addr(f.get(7)?)?,
                metric: f.get(6)?.parse().ok()?,
            })
        })
        .collect()
}

/// The interface the main routing table uses towards `target` (longest
/// prefix, then lowest metric).
fn route_interface_in(routes: &[Route], target: Ipv4Addr) -> Option<String> {
    routes
        .iter()
        .filter(|r| u32::from(target) & u32::from(r.mask) == u32::from(r.destination))
        .max_by(|a, b| {
            u32::from(a.mask)
                .count_ones()
                .cmp(&u32::from(b.mask).count_ones())
                .then(b.metric.cmp(&a.metric))
        })
        .map(|r| r.iface.clone())
}

/// The interface that leads to `target`.
pub fn route_interface(target: Ipv4Addr) -> Option<String> {
    let text = std::fs::read_to_string("/proc/net/route").ok()?;
    route_interface_in(&parse_routes(&text), target)
}

/// The interface of the default route.
pub fn default_interface() -> Option<String> {
    let text = std::fs::read_to_string("/proc/net/route").ok()?;
    parse_routes(&text)
        .into_iter()
        .filter(|r| r.mask.is_unspecified())
        .min_by_key(|r| r.metric)
        .map(|r| r.iface)
}

/// The source address this host uses towards `target` (no packet is sent).
pub fn source_address(target: Ipv4Addr) -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect(SocketAddrV4::new(target, 9)).ok()?;
    match socket.local_addr().ok()? {
        std::net::SocketAddr::V4(a) if !a.ip().is_unspecified() => Some(*a.ip()),
        _ => None,
    }
}

/// The address this site most likely has in the transport network: the
/// source address of the default route.
pub fn default_address() -> Option<Ipv4Addr> {
    source_address(Ipv4Addr::new(192, 0, 2, 1))
}

/// An IPv4 address of this host, with what it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostAddress {
    /// The interface that leads there.
    pub interface: String,
    /// The address.
    pub address: Ipv4Addr,
    /// The interface looks like a VPN (WireGuard, Tailscale, tun/tap,
    /// ZeroTier, Nebula).
    pub vpn: bool,
    /// It is the address of the default route.
    pub default_route: bool,
}

impl std::fmt::Display for HostAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let what = match (self.vpn, self.default_route) {
            (true, _) => ", VPN",
            (false, true) => ", default route",
            _ => "",
        };
        write!(f, "{} ({}{what})", self.address, self.interface)
    }
}

/// True if an interface of this name looks like a VPN.
pub fn is_vpn(interface: &str) -> bool {
    ["wg", "tailscale", "tun", "tap", "zt", "nebula"]
        .iter()
        .any(|p| interface.starts_with(p))
}

/// Interfaces of containers and of wanderlan itself: never the transport.
fn is_local_bridge(interface: &str) -> bool {
    ["docker", "br-", "virbr", "veth", "lxc", "cni", "vx", "lo"]
        .iter()
        .any(|p| interface.starts_with(p))
}

/// The addresses this host uses towards the networks of its routes: those
/// of VPN-like interfaces first, the one of the default route last (no
/// packet is sent).
pub fn host_addresses() -> Vec<HostAddress> {
    let Ok(text) = std::fs::read_to_string("/proc/net/route") else {
        return Vec::new();
    };
    let routes = parse_routes(&text);
    let default = default_address();
    let mut out: Vec<HostAddress> = Vec::new();
    for route in &routes {
        // A host of the network of the route, or one on the internet.
        let target = if route.mask.is_unspecified() {
            Ipv4Addr::new(192, 0, 2, 1)
        } else {
            Ipv4Addr::from(u32::from(route.destination).saturating_add(1))
        };
        let Some(interface) = route_interface_in(&routes, target) else {
            continue;
        };
        if is_local_bridge(&interface) {
            continue;
        }
        let Some(address) = source_address(target) else {
            continue;
        };
        if address.is_loopback() || out.iter().any(|h| h.address == address) {
            continue;
        }
        out.push(HostAddress {
            vpn: is_vpn(&interface),
            default_route: Some(address) == default,
            interface,
            address,
        });
    }
    out.sort_by_key(|h| (!h.vpn, h.default_route));
    out
}

/// Why `address` may be a poor choice as the address of this site in the
/// transport network (`known`: the addresses of this host).
pub fn address_warning(address: Ipv4Addr, known: &[HostAddress]) -> Option<String> {
    let [a, b, ..] = address.octets();
    let shared = a == 100 && (64..128).contains(&b);
    if !address.is_private() && !address.is_link_local() && !shared {
        return Some(format!(
            "{address} is a public address: the tunnels would carry the VLANs unencrypted across the internet; use the address of a VPN"
        ));
    }
    let host = known.iter().find(|h| h.address == address)?;
    (host.default_route && !host.vpn).then(|| {
        format!(
            "{address} is the address of the default route ({}), not of a VPN: the tunnels are not encrypted, fine only where the network between the sites is trusted",
            host.interface
        )
    })
}

/// The interfaces the traffic to the other sites leaves by: `via` (the one
/// of this site's address or of the route to its server) and the one of the
/// default route, which a VPN on this host runs over. As the trunk, either
/// would cut this host off.
pub fn transport_interfaces(via: Option<String>) -> Vec<String> {
    let mut out: Vec<String> = via.into_iter().collect();
    if let Some(way_out) = default_interface()
        && !out.contains(&way_out)
    {
        out.push(way_out);
    }
    out
}

/// Interfaces that can be the trunk, without those carrying the transport
/// traffic (`transport`).
pub fn trunk_candidates<'a>(
    interfaces: &'a [Interface],
    transport: &[String],
) -> Vec<&'a Interface> {
    let mut list: Vec<&Interface> = interfaces
        .iter()
        .filter(|i| i.is_trunk_candidate() && !transport.contains(&i.name))
        .collect();
    list.sort_by(|a, b| a.name.cmp(&b.name));
    list
}

/// The trunk if there is exactly one candidate, otherwise all candidates.
pub fn choose_trunk(interfaces: &[Interface], transport: &[String]) -> Result<String, Vec<String>> {
    let list = trunk_candidates(interfaces, transport);
    match list.as_slice() {
        [only] => Ok(only.name.clone()),
        many => Err(many.iter().map(|i| i.name.clone()).collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROUTES: &str = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
ens18\t00000000\t010D0A0A\t0003\t0\t0\t100\t00000000\t0\t0\t0\n\
ens18\t000D0A0A\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0\n\
wg0\t0000080A\t00000000\t0001\t0\t0\t0\t0000FFFF\t0\t0\t0\n\
dead0\t0000090A\t00000000\t0000\t0\t0\t0\t0000FFFF\t0\t0\t0\n";

    fn native(v: [u8; 4]) -> String {
        format!("{:08X}", u32::from_ne_bytes(v))
    }

    #[test]
    fn routes_pick_the_longest_prefix() {
        // The fixture is written for little-endian hosts; build it natively.
        let text = ROUTES
            .replace("000D0A0A", &native([10, 10, 13, 0]))
            .replace("00FFFFFF", &native([255, 255, 255, 0]))
            .replace("0000080A", &native([10, 8, 0, 0]))
            .replace("0000090A", &native([10, 9, 0, 0]))
            .replace("0000FFFF", &native([255, 255, 0, 0]));
        let routes = parse_routes(&text);
        assert_eq!(routes.len(), 3, "the route that is not up is skipped");
        let via = |a: [u8; 4]| route_interface_in(&routes, Ipv4Addr::from(a));
        assert_eq!(via([10, 10, 13, 104]).as_deref(), Some("ens18"));
        assert_eq!(via([10, 8, 0, 2]).as_deref(), Some("wg0"));
        assert_eq!(via([1, 1, 1, 1]).as_deref(), Some("ens18"));
        assert_eq!(via([10, 9, 0, 1]).as_deref(), Some("ens18"));
    }

    fn iface(name: &str, kind: &str, master: Option<&str>) -> Interface {
        Interface {
            name: name.into(),
            kind: kind.into(),
            up: true,
            master: master.map(Into::into),
            mtu: 1500,
            managed: kind == "vxlan",
            carries: Vec::new(),
        }
    }

    #[test]
    fn a_vpn_address_is_good_a_public_or_default_one_is_warned_about() {
        let host = |address: [u8; 4], interface: &str, default_route| HostAddress {
            interface: interface.into(),
            address: Ipv4Addr::from(address),
            vpn: is_vpn(interface),
            default_route,
        };
        let known = vec![
            host([10, 8, 0, 11], "wg0", false),
            host([192, 168, 1, 5], "ens18", true),
        ];
        assert_eq!(known[0].to_string(), "10.8.0.11 (wg0, VPN)");
        assert_eq!(known[1].to_string(), "192.168.1.5 (ens18, default route)");
        assert_eq!(address_warning(Ipv4Addr::new(10, 8, 0, 11), &known), None);
        assert_eq!(address_warning(Ipv4Addr::new(100, 64, 0, 7), &known), None);
        assert!(
            address_warning(Ipv4Addr::new(192, 168, 1, 5), &known)
                .is_some_and(|w| w.contains("default route (ens18)"))
        );
        assert!(
            address_warning(Ipv4Addr::new(203, 0, 113, 9), &known)
                .is_some_and(|w| w.contains("public address"))
        );
    }

    #[test]
    fn the_trunk_is_the_interface_that_does_not_carry_the_transport() {
        let interfaces = vec![
            iface("lo", "", None),
            iface("ens18", "", None),
            iface("ens19", "", None),
            iface("docker0", "bridge", None),
            iface("vx-a", "vxlan", Some("br0")),
        ];
        let ens18 = ["ens18".to_string()];
        assert_eq!(choose_trunk(&interfaces, &ens18), Ok("ens19".into()));
        assert_eq!(
            choose_trunk(&interfaces, &[]),
            Err(vec!["ens18".into(), "ens19".into()])
        );
        // The address is on wg0, which runs over ens18 (the default route).
        let both = ["wg0".to_string(), "ens18".to_string()];
        assert_eq!(choose_trunk(&interfaces, &both), Ok("ens19".into()));
        let attached = vec![iface("ens18", "", None), iface("ens19", "", Some("br0"))];
        assert_eq!(choose_trunk(&attached, &ens18), Ok("ens19".into()));
        let foreign = vec![iface("ens18", "", None), iface("ens19", "", Some("vmbr0"))];
        assert_eq!(choose_trunk(&foreign, &ens18), Err(vec![]));
    }
}
