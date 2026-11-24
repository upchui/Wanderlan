//! Layer-2 tunnel topology: servers and clients.
//!
//! A site without `uplink` is a **server**. All servers are connected with
//! each other (full mesh); loop freedom among them comes from split horizon:
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

use crate::config::{Mesh, Site};

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
/// that is not part of the mesh has none.
pub fn tunnels_for<'a>(mesh: &'a Mesh, local: &str) -> Vec<Tunnel<'a>> {
    let Some(me) = mesh.site(local) else {
        return Vec::new();
    };
    let mut out: Vec<Tunnel<'a>> = mesh
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

/// All tunnels of the mesh as unordered site pairs (smaller name first),
/// with `true` for tunnels between servers.
pub fn edges(mesh: &Mesh) -> BTreeSet<(String, String, bool)> {
    let mut out = BTreeSet::new();
    for site in &mesh.sites {
        for t in tunnels_for(mesh, &site.name) {
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
fn ancestors<'a>(mesh: &'a Mesh, site: &'a str) -> Vec<&'a str> {
    let mut chain = vec![site];
    let mut current = site;
    while let Some(up) = mesh.site(current).and_then(|s| s.uplink.as_deref()) {
        if chain.contains(&up) || mesh.site(up).is_none() {
            break;
        }
        chain.push(up);
        current = up;
    }
    chain
}

/// The neighbour through which `from` reaches `to`, if `to` is neither
/// `from` itself nor a direct neighbour: shown as "via …".
pub fn via<'a>(mesh: &'a Mesh, from: &str, to: &str) -> Option<&'a str> {
    let from = mesh.site(from)?.name.as_str();
    let to = mesh.site(to)?.name.as_str();
    if from == to {
        return None;
    }
    let up_from_target = ancestors(mesh, to);
    let next = match up_from_target.iter().position(|s| *s == from) {
        // `to` hangs (indirectly) off `from`: towards that client.
        Some(pos) => *up_from_target.get(pos.checked_sub(1)?)?,
        None => match mesh.site(from).and_then(|s| s.uplink.as_deref()) {
            // Up towards the own server.
            Some(up) => up,
            // A server: to the server `to` hangs off.
            None => *up_from_target.last()?,
        },
    };
    (next != to).then_some(next)
}

/// Everything about the mesh that determines over which ports frames
/// travel: the tunnels and whether they connect servers. Two meshes with the
/// same key have the same layer-2 paths, so learned MAC addresses stay
/// valid; VLAN changes do not move addresses between ports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardingKey {
    edges: BTreeSet<(String, String, bool)>,
}

/// Computes the [`ForwardingKey`] of a mesh.
pub fn forwarding_key(mesh: &Mesh) -> ForwardingKey {
    ForwardingKey { edges: edges(mesh) }
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

    fn example() -> Mesh {
        let mut mesh = Mesh::new(VlanSet::from_range(10, 50));
        mesh.sites = vec![site("heim", 1), site("buero", 2), site("haus2", 11)];
        mesh
    }

    /// Two servers; alex has the clients marcel and site2, marcel has x.
    fn with_clients() -> Mesh {
        let mut mesh = Mesh::new(VlanSet::from_range(10, 50));
        mesh.sites = vec![
            site("alex", 1),
            site("dc", 2),
            client("marcel", 3, "alex"),
            client("site2", 4, "alex"),
            client("x", 5, "marcel"),
        ];
        mesh
    }

    fn peers(mesh: &Mesh, site: &str) -> Vec<(String, TunnelKind)> {
        tunnels_for(mesh, site)
            .iter()
            .map(|t| (t.peer.name.clone(), t.kind))
            .collect()
    }

    #[test]
    fn servers_have_a_tunnel_to_every_other_server() {
        let mesh = example();
        let names: Vec<String> = tunnels_for(&mesh, "heim")
            .iter()
            .map(Tunnel::ifname)
            .collect();
        assert_eq!(names, vec!["gt-buero", "gt-haus2"]);
        assert!(tunnels_for(&mesh, "heim").iter().all(Tunnel::isolated));
        assert_eq!(tunnels_for(&mesh, "haus2").len(), 2);
        assert!(tunnels_for(&mesh, "unknown").is_empty());
    }

    #[test]
    fn a_client_has_one_tunnel_to_its_uplink_and_none_to_other_clients() {
        let mesh = with_clients();
        use TunnelKind::{Client, Server, Uplink};
        assert_eq!(
            peers(&mesh, "alex"),
            [
                ("dc".to_string(), Server),
                ("marcel".to_string(), Client),
                ("site2".to_string(), Client)
            ]
        );
        assert_eq!(
            peers(&mesh, "marcel"),
            [("alex".to_string(), Uplink), ("x".to_string(), Client)]
        );
        assert_eq!(peers(&mesh, "site2"), [("alex".to_string(), Uplink)]);
        assert_eq!(peers(&mesh, "x"), [("marcel".to_string(), Uplink)]);
        assert_eq!(peers(&mesh, "dc"), [("alex".to_string(), Server)]);
        assert!(
            !tunnels_for(&mesh, "marcel").iter().any(Tunnel::isolated),
            "client ports are ordinary ports"
        );
    }

    #[test]
    fn via_names_the_next_hop() {
        let mesh = with_clients();
        assert_eq!(via(&mesh, "marcel", "site2"), Some("alex"));
        assert_eq!(via(&mesh, "marcel", "dc"), Some("alex"));
        assert_eq!(via(&mesh, "marcel", "alex"), None, "direct");
        assert_eq!(via(&mesh, "alex", "x"), Some("marcel"));
        assert_eq!(via(&mesh, "dc", "x"), Some("alex"));
        assert_eq!(via(&mesh, "dc", "site2"), Some("alex"));
        assert_eq!(via(&mesh, "x", "site2"), Some("marcel"));
        assert_eq!(via(&mesh, "x", "x"), None);
        assert_eq!(via(&mesh, "x", "nowhere"), None);
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
        let mut mesh = Mesh::new(VlanSet::from_range(10, 50));
        mesh.sites = vec![site("first", 1)];
        assert!(tunnels_for(&mesh, "first").is_empty());
        assert!(edges(&mesh).is_empty());
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
}
