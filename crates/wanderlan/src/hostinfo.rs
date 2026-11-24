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

/// Interfaces that can be the trunk, without the one carrying the transport
/// traffic (`transport`).
pub fn trunk_candidates<'a>(
    interfaces: &'a [Interface],
    transport: Option<&str>,
) -> Vec<&'a Interface> {
    let mut list: Vec<&Interface> = interfaces
        .iter()
        .filter(|i| i.is_trunk_candidate() && Some(i.name.as_str()) != transport)
        .collect();
    list.sort_by(|a, b| a.name.cmp(&b.name));
    list
}

/// The trunk if there is exactly one candidate, otherwise all candidates.
pub fn choose_trunk(
    interfaces: &[Interface],
    transport: Option<&str>,
) -> Result<String, Vec<String>> {
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
            managed: kind == "gretap",
        }
    }

    #[test]
    fn the_trunk_is_the_interface_that_does_not_carry_the_transport() {
        let interfaces = vec![
            iface("lo", "", None),
            iface("ens18", "", None),
            iface("ens19", "", None),
            iface("docker0", "bridge", None),
            iface("gt-a", "gretap", Some("br0")),
        ];
        assert_eq!(choose_trunk(&interfaces, Some("ens18")), Ok("ens19".into()));
        assert_eq!(
            choose_trunk(&interfaces, None),
            Err(vec!["ens18".into(), "ens19".into()])
        );
        let attached = vec![iface("ens18", "", None), iface("ens19", "", Some("br0"))];
        assert_eq!(choose_trunk(&attached, Some("ens18")), Ok("ens19".into()));
        let foreign = vec![iface("ens18", "", None), iface("ens19", "", Some("vmbr0"))];
        assert_eq!(choose_trunk(&foreign, Some("ens18")), Err(vec![]));
    }
}
