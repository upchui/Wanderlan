//! Layer-2 tunnel topology: which sites get a GRETAP tunnel to which.
//!
//! Hubs are fully meshed, every spoke has exactly one tunnel to its hub.
//! Loop freedom comes from split horizon: on a hub, tunnels to other hubs are
//! marked `isolated`, so frames received from one hub are never forwarded to
//! another hub. Frames from spokes may go anywhere.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::config::{ConfigFile, Diagnostic, FieldPath, Mesh, Role, Site};

/// Prefix of the GRETAP interfaces created by wanderlan.
pub const GRETAP_PREFIX: &str = "gt-";

/// Kind of a tunnel, seen from the local site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TunnelKind {
    /// Local hub to another hub.
    HubToHub,
    /// Local hub to one of its spokes.
    HubToSpoke,
    /// Local spoke to its hub.
    SpokeToHub,
}

/// A tunnel from the local site to a peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tunnel<'a> {
    /// The remote site.
    pub peer: &'a Site,
    /// Kind of the tunnel.
    pub kind: TunnelKind,
}

impl Tunnel<'_> {
    /// Name of the GRETAP interface for this tunnel.
    pub fn ifname(&self) -> String {
        gretap_name(&self.peer.name)
    }

    /// Whether the bridge port must be isolated (split horizon).
    pub fn isolated(&self) -> bool {
        self.kind == TunnelKind::HubToHub
    }
}

/// Name of the GRETAP interface towards a peer.
pub fn gretap_name(peer: &str) -> String {
    format!("{GRETAP_PREFIX}{peer}")
}

/// Returns the tunnels the given site has to build, sorted by peer name.
///
/// Sites with invalid references (e.g. a spoke naming a missing hub) are
/// skipped; [`check`] reports them.
pub fn tunnels_for<'a>(mesh: &'a Mesh, local: &str) -> Vec<Tunnel<'a>> {
    let Some(me) = mesh.site(local) else {
        return Vec::new();
    };
    let mut out: Vec<Tunnel<'a>> = match me.role {
        Role::Hub => mesh
            .sites
            .iter()
            .filter(|s| s.name != me.name)
            .filter_map(|s| match s.role {
                Role::Hub => Some(Tunnel {
                    peer: s,
                    kind: TunnelKind::HubToHub,
                }),
                Role::Spoke if s.hub.as_deref() == Some(me.name.as_str()) => Some(Tunnel {
                    peer: s,
                    kind: TunnelKind::HubToSpoke,
                }),
                Role::Spoke => None,
            })
            .collect(),
        Role::Spoke => me
            .hub
            .as_deref()
            .and_then(|h| mesh.site(h))
            .filter(|h| h.role == Role::Hub && h.name != me.name)
            .map(|h| Tunnel {
                peer: h,
                kind: TunnelKind::SpokeToHub,
            })
            .into_iter()
            .collect(),
    };
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

/// Checks the topology invariants that make split horizon loop-free:
/// hubs are fully meshed, every spoke has exactly one tunnel and it leads to a
/// hub, there are no spoke-to-spoke tunnels and the mesh is connected.
///
/// Errors about the referenced hub itself (missing, not a hub) are reported
/// by the configuration validator; this function only reports what remains.
pub fn check(mesh: &Mesh) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    let index: BTreeMap<&str, usize> = mesh
        .sites
        .iter()
        .enumerate()
        .map(|(i, s)| (s.name.as_str(), i))
        .collect();
    let role_of = |name: &str| {
        index
            .get(name)
            .and_then(|i| mesh.sites.get(*i))
            .map(|s| s.role)
    };

    let edges = edges(mesh);
    let mut adjacency: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (a, b) in &edges {
        adjacency.entry(a).or_default().insert(b);
        adjacency.entry(b).or_default().insert(a);
    }

    let hubs: Vec<&Site> = mesh.hubs().collect();
    for (i, a) in hubs.iter().enumerate() {
        for b in hubs.iter().skip(i + 1) {
            let linked = adjacency
                .get(a.name.as_str())
                .is_some_and(|n| n.contains(b.name.as_str()));
            if !linked {
                diags.push(Diagnostic::error(
                    ConfigFile::Mesh,
                    FieldPath::root(),
                    format!(
                        "hubs \"{}\" and \"{}\" are not connected; hubs must form a full mesh",
                        a.name, b.name
                    ),
                ));
            }
        }
    }

    for (i, site) in mesh.sites.iter().enumerate() {
        if site.role != Role::Spoke {
            continue;
        }
        let neighbours = adjacency.get(site.name.as_str());
        let count = neighbours.map_or(0, BTreeSet::len);
        if count > 1 {
            diags.push(Diagnostic::error(
                ConfigFile::Mesh,
                FieldPath::site(i),
                format!(
                    "spoke \"{}\" would have {count} tunnels; a spoke must be connected to exactly one hub",
                    site.name
                ),
            ));
        }
        for n in neighbours.into_iter().flatten() {
            if role_of(n) == Some(Role::Spoke) {
                diags.push(Diagnostic::error(
                    ConfigFile::Mesh,
                    FieldPath::site(i),
                    format!(
                        "spokes \"{}\" and \"{n}\" must not be connected directly",
                        site.name
                    ),
                ));
            }
        }
    }

    // Connectivity: every site must be reachable from the first hub. Spokes
    // with a broken hub reference are already reported by the validator.
    if let Some(start) = hubs.first() {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        let mut queue = VecDeque::from([start.name.as_str()]);
        while let Some(n) = queue.pop_front() {
            if seen.insert(n) {
                queue.extend(adjacency.get(n).into_iter().flatten().copied());
            }
        }
        for (i, site) in mesh.sites.iter().enumerate() {
            let hub_ok = site.role == Role::Hub
                || site
                    .hub
                    .as_deref()
                    .is_some_and(|h| role_of(h) == Some(Role::Hub));
            if hub_ok && !seen.contains(site.name.as_str()) {
                diags.push(Diagnostic::error(
                    ConfigFile::Mesh,
                    FieldPath::site(i),
                    format!(
                        "site \"{}\" is not connected to the rest of the mesh",
                        site.name
                    ),
                ));
            }
        }
    }
    diags
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;
    use crate::config::VlanSet;

    fn site(name: &str, role: Role, hub: Option<&str>, last: u8) -> Site {
        Site {
            name: name.into(),
            role,
            hub: hub.map(Into::into),
            address: Ipv4Addr::new(10, 0, 0, last),
        }
    }

    fn example() -> Mesh {
        let mut mesh = Mesh::new(VlanSet::from_range(10, 50));
        mesh.sites = vec![
            site("heim", Role::Hub, None, 1),
            site("buero", Role::Hub, None, 2),
            site("haus2", Role::Spoke, Some("heim"), 11),
            site("haus3", Role::Spoke, Some("buero"), 12),
            site("haus4", Role::Spoke, Some("heim"), 13),
        ];
        mesh
    }

    fn names(tunnels: &[Tunnel<'_>]) -> Vec<(String, TunnelKind, bool)> {
        tunnels
            .iter()
            .map(|t| (t.ifname(), t.kind, t.isolated()))
            .collect()
    }

    #[test]
    fn hub_tunnels_and_isolation() {
        let mesh = example();
        assert_eq!(
            names(&tunnels_for(&mesh, "heim")),
            vec![
                ("gt-buero".into(), TunnelKind::HubToHub, true),
                ("gt-haus2".into(), TunnelKind::HubToSpoke, false),
                ("gt-haus4".into(), TunnelKind::HubToSpoke, false),
            ]
        );
    }

    #[test]
    fn spoke_has_exactly_its_hub() {
        let mesh = example();
        assert_eq!(
            names(&tunnels_for(&mesh, "haus3")),
            vec![("gt-buero".into(), TunnelKind::SpokeToHub, false)]
        );
        assert!(tunnels_for(&mesh, "unknown").is_empty());
    }

    #[test]
    fn edges_are_symmetric() {
        let mesh = example();
        let e = edges(&mesh);
        assert_eq!(e.len(), 4);
        assert!(e.contains(&("buero".to_string(), "heim".to_string())));
        assert!(e.contains(&("haus2".to_string(), "heim".to_string())));
    }

    #[test]
    fn valid_topology_has_no_findings() {
        assert!(check(&example()).is_empty());
    }

    #[test]
    fn spoke_with_missing_hub_is_left_to_validator() {
        let mut mesh = example();
        mesh.sites[2].hub = Some("nowhere".into());
        assert!(check(&mesh).is_empty());
        assert!(tunnels_for(&mesh, "haus2").is_empty());
    }

    #[test]
    fn two_hubs_only_is_the_simple_case() {
        let mut mesh = Mesh::new(VlanSet::from_range(10, 50));
        mesh.sites = vec![site("a", Role::Hub, None, 1), site("b", Role::Hub, None, 2)];
        assert!(check(&mesh).is_empty());
        assert_eq!(edges(&mesh).len(), 1);
    }
}
