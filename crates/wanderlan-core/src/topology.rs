//! Layer-2 tunnel topology: servers and clients.
//!
//! A site without `uplink` is a **server**. All servers are connected with
//! each other directly; loop freedom among them comes from split horizon:
//! server-to-server ports are `isolated`, so a frame received from one server
//! is never forwarded to another server, only to the local trunk and to
//! clients.
//!
//! A site with `uplink` is a **client** of that site, plugged into it like a
//! cable into a switch: its only tunnel leads there, and that port is an
//! ordinary bridge port. Clients of the same site have no tunnel to each
//! other. Whether two sites reach each other is decided by the VLANs alone:
//! every tunnel carries the VLANs both of its ends take part in, so two
//! clients meet only in a VLAN that they and the site they are plugged into
//! all have.
//!
//! Servers must reach each other in the transport network; a client only
//! needs to reach its uplink.

use std::collections::BTreeSet;

use crate::config::{Network, Site, VlanSet};

/// Prefix of the GRETAP interfaces created by wanderlan.
pub const GRETAP_PREFIX: &str = "gt-";

/// What a tunnel connects, seen from the local site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TunnelKind {
    /// Between two servers (isolated ports, split horizon).
    Server,
    /// To the site this site is a client of.
    Uplink,
    /// To a client of this site.
    Client,
}

/// A tunnel from the local site to a peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tunnel<'a> {
    /// The remote site.
    pub peer: &'a Site,
    /// What the tunnel connects.
    pub kind: TunnelKind,
}

impl Tunnel<'_> {
    /// Name of the GRETAP interface for this tunnel.
    pub fn ifname(&self) -> String {
        gretap_name(&self.peer.name)
    }

    /// Server-to-server ports are isolated (split horizon); the ports of a
    /// client and its uplink are ordinary bridge ports.
    pub fn isolated(&self) -> bool {
        self.kind == TunnelKind::Server
    }
}

/// Name of the GRETAP interface towards a peer.
pub fn gretap_name(peer: &str) -> String {
    format!("{GRETAP_PREFIX}{peer}")
}

/// True for a server (a site without uplink).
pub fn is_server(site: &Site) -> bool {
    site.uplink.is_none()
}

/// The tunnels the given site has to build, sorted by peer name: to its
/// uplink, to its clients and, as a server, to every other server. A site
/// that is not part of the network has none.
pub fn tunnels_for<'a>(network: &'a Network, local: &str) -> Vec<Tunnel<'a>> {
    let Some(me) = network.site(local) else {
        return Vec::new();
    };
    let mut out: Vec<Tunnel<'a>> = network
        .sites
        .iter()
        .filter(|peer| peer.name != local)
        .filter_map(|peer| {
            let kind = if me.uplink.as_deref() == Some(peer.name.as_str()) {
                TunnelKind::Uplink
            } else if peer.uplink.as_deref() == Some(local) {
                TunnelKind::Client
            } else if is_server(me) && is_server(peer) {
                TunnelKind::Server
            } else {
                return None;
            };
            Some(Tunnel { peer, kind })
        })
        .collect();
    out.sort_by(|a, b| a.peer.name.cmp(&b.peer.name));
    out
}

/// The VLANs the tunnel between `a` and `b` carries, or `None` if there is
/// no tunnel between them. Between servers: the VLANs both take part in.
/// Between a site and its client: also the own VLANs of the upper site,
/// never those of the client. Never more than the tunnel's own limit
/// (`[[tunnel]]`).
pub fn tunnel_vlans(network: &Network, a: &str, b: &str) -> Option<VlanSet> {
    let me = network.site(a)?;
    let tunnel = tunnels_for(network, a)
        .into_iter()
        .find(|t| t.peer.name == b)?;
    let peer = tunnel.peer;
    let with_own = |s: &Site| -> VlanSet {
        network
            .effective_vlans(s)
            .iter()
            .chain(network.own_vlans(s).iter())
            .collect()
    };
    let (ours, theirs) = match tunnel.kind {
        TunnelKind::Client => (with_own(me), network.effective_vlans(peer)),
        TunnelKind::Uplink => (network.effective_vlans(me), with_own(peer)),
        TunnelKind::Server => (network.effective_vlans(me), network.effective_vlans(peer)),
    };
    let vlans = ours.intersection(&theirs);
    Some(match network.tunnel_limit(a, b) {
        Some(limit) => vlans.intersection(limit),
        None => vlans,
    })
}

/// All tunnels of the network as unordered site pairs (smaller name first),
/// with `true` for tunnels between servers.
pub fn edges(network: &Network) -> BTreeSet<(String, String, bool)> {
    let mut out = BTreeSet::new();
    for site in &network.sites {
        for t in tunnels_for(network, &site.name) {
            let (a, b) = if site.name <= t.peer.name {
                (site.name.clone(), t.peer.name.clone())
            } else {
                (t.peer.name.clone(), site.name.clone())
            };
            out.insert((a, b, t.kind == TunnelKind::Server));
        }
    }
    out
}

/// The chain from `site` up to its server: the site, its uplink, the uplink
/// of that, ... Stops at a loop or an unknown site (invalid configurations).
fn ancestors<'a>(network: &'a Network, site: &'a str) -> Vec<&'a str> {
    let mut chain = vec![site];
    let mut current = site;
    while let Some(up) = network.site(current).and_then(|s| s.uplink.as_deref()) {
        if chain.contains(&up) || network.site(up).is_none() {
            break;
        }
        chain.push(up);
        current = up;
    }
    chain
}

/// The neighbour through which `from` reaches `to`, if `to` is neither
/// `from` itself nor a direct neighbour: shown as "via …".
pub fn via<'a>(network: &'a Network, from: &str, to: &str) -> Option<&'a str> {
    let from = network.site(from)?.name.as_str();
    let to = network.site(to)?.name.as_str();
    if from == to {
        return None;
    }
    let up_from_target = ancestors(network, to);
    let next = match up_from_target.iter().position(|s| *s == from) {
        // `to` hangs (indirectly) off `from`: towards that client.
        Some(pos) => *up_from_target.get(pos.checked_sub(1)?)?,
        None => match network.site(from).and_then(|s| s.uplink.as_deref()) {
            // Up towards the own server.
            Some(up) => up,
            // A server: to the server `to` hangs off.
            None => *up_from_target.last()?,
        },
    };
    (next != to).then_some(next)
}

/// Display order of sites given as `(name, uplink)`: every server (or site
/// whose uplink is unknown here), each followed by its clients, recursively.
/// Returns `(index, depth)`; sites in an uplink loop come last at depth 0.
pub fn tree_order_by<'a>(sites: &[(&'a str, Option<&'a str>)]) -> Vec<(usize, usize)> {
    fn add<'a>(
        sites: &[(&'a str, Option<&'a str>)],
        index: usize,
        depth: usize,
        seen: &mut Vec<bool>,
        out: &mut Vec<(usize, usize)>,
    ) {
        if seen.get(index).copied().unwrap_or(true) {
            return;
        }
        if let Some(s) = seen.get_mut(index) {
            *s = true;
        }
        out.push((index, depth));
        let name = sites.get(index).map(|s| s.0);
        for (child, (_, uplink)) in sites.iter().enumerate() {
            if uplink.is_some() && *uplink == name {
                add(sites, child, depth + 1, seen, out);
            }
        }
    }
    let mut seen = vec![false; sites.len()];
    let mut out = Vec::with_capacity(sites.len());
    for (i, (_, uplink)) in sites.iter().enumerate() {
        let root = uplink.is_none_or(|u| !sites.iter().any(|(n, _)| *n == u));
        if root {
            add(sites, i, 0, &mut seen, &mut out);
        }
    }
    for i in 0..sites.len() {
        if !seen.get(i).copied().unwrap_or(true) {
            add(sites, i, 0, &mut seen, &mut out);
        }
    }
    out
}

/// A site name indented below its server: `alex`, `└ marcel`, `  └ x`.
pub fn tree_name(name: &str, depth: usize) -> String {
    match depth {
        0 => name.to_string(),
        d => format!("{}└ {name}", "  ".repeat(d - 1)),
    }
}

/// [`tree_order_by`] for the sites of a network.
pub fn tree_order(network: &Network) -> Vec<(usize, usize)> {
    let sites: Vec<(&str, Option<&str>)> = network
        .sites
        .iter()
        .map(|s| (s.name.as_str(), s.uplink.as_deref()))
        .collect();
    tree_order_by(&sites)
}

/// Everything about the network that determines over which ports frames
/// travel: the tunnels and whether they connect servers. Two networks with the
/// same key have the same layer-2 paths, so learned MAC addresses stay
/// valid; VLAN changes do not move addresses between ports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardingKey {
    edges: BTreeSet<(String, String, bool)>,
}

/// Computes the [`ForwardingKey`] of a network.
pub fn forwarding_key(network: &Network) -> ForwardingKey {
    ForwardingKey {
        edges: edges(network),
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;
    use crate::config::VlanSet;

    fn site(name: &str, last: u8) -> Site {
        Site {
            name: name.into(),
            address: Ipv4Addr::new(10, 0, 0, last),
            vlans: None,
            own_vlans: None,
            online: true,
            managed_by: None,
            self_managed: false,
            uplink: None,
        }
    }

    fn client(name: &str, last: u8, uplink: &str) -> Site {
        Site {
            uplink: Some(uplink.into()),
            ..site(name, last)
        }
    }

    fn example() -> Network {
        let mut network = Network::new(VlanSet::from_range(10, 50));
        network.sites = vec![site("heim", 1), site("buero", 2), site("haus2", 11)];
        network
    }

    /// Two servers; alex has the clients marcel and site2, marcel has x.
    fn with_clients() -> Network {
        let mut network = Network::new(VlanSet::from_range(10, 50));
        network.sites = vec![
            site("alex", 1),
            site("dc", 2),
            client("marcel", 3, "alex"),
            client("site2", 4, "alex"),
            client("x", 5, "marcel"),
        ];
        network
    }

    fn peers(network: &Network, site: &str) -> Vec<(String, TunnelKind)> {
        tunnels_for(network, site)
            .iter()
            .map(|t| (t.peer.name.clone(), t.kind))
            .collect()
    }

    #[test]
    fn servers_have_a_tunnel_to_every_other_server() {
        let network = example();
        let names: Vec<String> = tunnels_for(&network, "heim")
            .iter()
            .map(Tunnel::ifname)
            .collect();
        assert_eq!(names, vec!["gt-buero", "gt-haus2"]);
        assert!(tunnels_for(&network, "heim").iter().all(Tunnel::isolated));
        assert_eq!(tunnels_for(&network, "haus2").len(), 2);
        assert!(tunnels_for(&network, "unknown").is_empty());
    }

    #[test]
    fn a_client_has_one_tunnel_to_its_uplink_and_none_to_other_clients() {
        let network = with_clients();
        use TunnelKind::{Client, Server, Uplink};
        assert_eq!(
            peers(&network, "alex"),
            [
                ("dc".to_string(), Server),
                ("marcel".to_string(), Client),
                ("site2".to_string(), Client)
            ]
        );
        assert_eq!(
            peers(&network, "marcel"),
            [("alex".to_string(), Uplink), ("x".to_string(), Client)]
        );
        assert_eq!(peers(&network, "site2"), [("alex".to_string(), Uplink)]);
        assert_eq!(peers(&network, "x"), [("marcel".to_string(), Uplink)]);
        assert_eq!(peers(&network, "dc"), [("alex".to_string(), Server)]);
        assert!(
            !tunnels_for(&network, "marcel").iter().any(Tunnel::isolated),
            "client ports are ordinary ports"
        );
    }

    #[test]
    fn via_names_the_next_hop() {
        let network = with_clients();
        assert_eq!(via(&network, "marcel", "site2"), Some("alex"));
        assert_eq!(via(&network, "marcel", "dc"), Some("alex"));
        assert_eq!(via(&network, "marcel", "alex"), None, "direct");
        assert_eq!(via(&network, "alex", "x"), Some("marcel"));
        assert_eq!(via(&network, "dc", "x"), Some("alex"));
        assert_eq!(via(&network, "dc", "site2"), Some("alex"));
        assert_eq!(via(&network, "x", "site2"), Some("marcel"));
        assert_eq!(via(&network, "x", "x"), None);
        assert_eq!(via(&network, "x", "nowhere"), None);
    }

    #[test]
    fn the_tree_order_puts_clients_under_their_server() {
        let network = with_clients();
        let order: Vec<(&str, usize)> = tree_order(&network)
            .into_iter()
            .map(|(i, d)| (network.sites[i].name.as_str(), d))
            .collect();
        assert_eq!(
            order,
            [
                ("alex", 0),
                ("marcel", 1),
                ("x", 2),
                ("site2", 1),
                ("dc", 0)
            ]
        );
        // A loop does not hang and nothing is lost.
        let looped = [("a", Some("b")), ("b", Some("a")), ("c", None)];
        assert_eq!(tree_order_by(&looped).len(), 3);
        // A client's view: its server (no uplink there) is the root.
        let view = [("site2", Some("alex")), ("alex", None)];
        assert_eq!(tree_order_by(&view), [(1, 0), (0, 1)]);
        assert_eq!(tree_name("alex", 0), "alex");
        assert_eq!(tree_name("marcel", 1), "└ marcel");
        assert_eq!(tree_name("x", 2), "  └ x");
    }

    #[test]
    fn edges_are_symmetric_and_complete() {
        let e = edges(&example());
        assert_eq!(e.len(), 3);
        assert!(e.contains(&("buero".to_string(), "heim".to_string(), true)));
        assert!(e.contains(&("buero".to_string(), "haus2".to_string(), true)));
        assert!(e.contains(&("haus2".to_string(), "heim".to_string(), true)));
        let e = edges(&with_clients());
        assert_eq!(e.len(), 4);
        assert!(e.contains(&("alex".to_string(), "marcel".to_string(), false)));
        assert!(!e.iter().any(|(a, b, _)| a == "marcel" && b == "site2"));
    }

    #[test]
    fn a_single_site_has_no_tunnels() {
        let mut network = Network::new(VlanSet::from_range(10, 50));
        network.sites = vec![site("first", 1)];
        assert!(tunnels_for(&network, "first").is_empty());
        assert!(edges(&network).is_empty());
    }

    #[test]
    fn forwarding_key_follows_the_tunnels_only() {
        let a = example();
        let mut b = example();
        b.sites[0].address = Ipv4Addr::new(10, 0, 0, 99);
        b.sites[1].vlans = Some(VlanSet::from_range(10, 20));
        assert_eq!(forwarding_key(&a), forwarding_key(&b));
        b.sites.pop();
        assert_ne!(forwarding_key(&a), forwarding_key(&b));
        // A server that becomes a client changes the paths.
        let mut c = example();
        c.sites[2].uplink = Some("heim".into());
        assert_ne!(forwarding_key(&a), forwarding_key(&c));
    }

    #[test]
    fn a_tunnel_carries_at_most_its_limit() {
        let mut network = with_clients();
        // dc takes part in 30-60.
        network.sites[1].vlans = Some(VlanSet::from_range(30, 60));
        assert_eq!(
            tunnel_vlans(&network, "alex", "dc"),
            Some(VlanSet::from_range(30, 50)),
            "servers: what both have"
        );
        // Down to a client also the own VLANs of the upper site.
        network.sites[0].own_vlans = Some(VlanSet::from_range(60, 60));
        network.sites[2].vlans = Some(VlanSet::from_range(10, 60));
        assert!(
            tunnel_vlans(&network, "alex", "marcel")
                .unwrap()
                .contains(60)
        );
        assert_eq!(
            tunnel_vlans(&network, "marcel", "alex"),
            tunnel_vlans(&network, "alex", "marcel"),
            "the same from both ends"
        );
        // The limit narrows it, entered in either order.
        network.set_tunnel_limit("dc", "alex", Some(VlanSet::from_range(40, 45)));
        assert_eq!(
            tunnel_vlans(&network, "alex", "dc"),
            Some(VlanSet::from_range(40, 45))
        );
        network.set_tunnel_limit("alex", "dc", Some(VlanSet::new()));
        assert_eq!(tunnel_vlans(&network, "dc", "alex"), Some(VlanSet::new()));
        assert_eq!(network.tunnels.len(), 1);
        assert_eq!(
            network.tunnels[0].sites,
            ["alex".to_string(), "dc".to_string()]
        );
        // Two clients of one site have no tunnel.
        assert_eq!(tunnel_vlans(&network, "marcel", "site2"), None);
        network.set_tunnel_limit("alex", "dc", None);
        assert!(network.tunnels.is_empty());
    }
}
