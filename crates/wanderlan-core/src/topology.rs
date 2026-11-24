//! Layer-2 tunnel topology: every site has a GRETAP tunnel to every other
//! site.
//!
//! All sites together act as one virtual switch; every tunnel is a port of
//! it. Loop freedom comes from split horizon: all tunnel ports are marked
//! `isolated`, so a frame received from one tunnel is only forwarded to the
//! local trunk, never into another tunnel. This needs every site to reach
//! every other site in the transport network.

use std::collections::BTreeSet;

use crate::config::{Mesh, Site};

/// Prefix of the GRETAP interfaces created by wanderlan.
pub const GRETAP_PREFIX: &str = "gt-";

/// A tunnel from the local site to a peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tunnel<'a> {
    /// The remote site.
    pub peer: &'a Site,
}

impl Tunnel<'_> {
    /// Name of the GRETAP interface for this tunnel.
    pub fn ifname(&self) -> String {
        gretap_name(&self.peer.name)
    }
}

/// Name of the GRETAP interface towards a peer.
pub fn gretap_name(peer: &str) -> String {
    format!("{GRETAP_PREFIX}{peer}")
}

/// Returns the tunnels the given site has to build (one to every other
/// site), sorted by peer name. A site that is not part of the mesh has none.
pub fn tunnels_for<'a>(mesh: &'a Mesh, local: &str) -> Vec<Tunnel<'a>> {
    if mesh.site(local).is_none() {
        return Vec::new();
    }
    let mut out: Vec<Tunnel<'a>> = mesh
        .sites
        .iter()
        .filter(|s| s.name != local)
        .map(|peer| Tunnel { peer })
        .collect();
    out.sort_by(|a, b| a.peer.name.cmp(&b.peer.name));
    out
}

/// All tunnels of the mesh as unordered site pairs (smaller name first).
pub fn edges(mesh: &Mesh) -> BTreeSet<(String, String)> {
    let mut out = BTreeSet::new();
    for site in &mesh.sites {
        for t in tunnels_for(mesh, &site.name) {
            let (a, b) = if site.name <= t.peer.name {
                (site.name.clone(), t.peer.name.clone())
            } else {
                (t.peer.name.clone(), site.name.clone())
            };
            out.insert((a, b));
        }
    }
    out
}

/// Everything about the mesh that determines over which ports frames
/// travel: the tunnels. Two meshes with the same key have the same layer-2
/// paths, so learned MAC addresses stay valid; VLAN changes do not move
/// addresses between ports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardingKey {
    edges: BTreeSet<(String, String)>,
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
        }
    }

    fn example() -> Mesh {
        let mut mesh = Mesh::new(VlanSet::from_range(10, 50));
        mesh.sites = vec![site("heim", 1), site("buero", 2), site("haus2", 11)];
        mesh
    }

    #[test]
    fn every_site_has_a_tunnel_to_every_other_site() {
        let mesh = example();
        let names: Vec<String> = tunnels_for(&mesh, "heim")
            .iter()
            .map(Tunnel::ifname)
            .collect();
        assert_eq!(names, vec!["gt-buero", "gt-haus2"]);
        assert_eq!(tunnels_for(&mesh, "haus2").len(), 2);
        assert!(tunnels_for(&mesh, "unknown").is_empty());
    }

    #[test]
    fn edges_are_symmetric_and_complete() {
        let e = edges(&example());
        assert_eq!(e.len(), 3);
        assert!(e.contains(&("buero".to_string(), "heim".to_string())));
        assert!(e.contains(&("buero".to_string(), "haus2".to_string())));
        assert!(e.contains(&("haus2".to_string(), "heim".to_string())));
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
    }
}
