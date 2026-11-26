//! Which tunnels of a site pass a VLAN on to each other.
//!
//! Per VLAN, the tunnels that carry it form a graph. Its **blocks** (the
//! biconnected components) decide at every site:
//!
//! * two tunnels of a site in the **same block** do not pass the VLAN on to
//!   each other (split horizon): inside a block every pair of sites has its
//!   own tunnel, or the block is a ring, where passing on would loop;
//! * two tunnels in **different blocks** do: the site is the only way
//!   between them (e.g. `b` between `a – b` and `b – c`).
//!
//! A frame thus crosses every block in one hop and follows the tree of
//! blocks between them, so it never comes back: no loops, whatever the
//! limits of the tunnels are. Without limits all servers have tunnels with
//! each other (one block: the split horizon of the servers) and every client
//! tunnel is a block of its own (passed on like an ordinary switch port).
//!
//! The kernel's port flag `isolated` expresses the usual case (all server
//! tunnels of a site in one block for every VLAN). Only a site that passes
//! a VLAN on between server tunnels needs nftables rules instead.

use std::collections::{BTreeMap, BTreeSet};

use crate::config::{Network, VID_MAX, VID_MIN, VlanSet};
use crate::topology;

/// How a site keeps the split horizon.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Forwarding {
    /// Peers whose tunnel port is `isolated` in the kernel.
    pub isolated: BTreeSet<String>,
    /// Instead of `isolated`: the tunnels (by peer) that must not pass the
    /// VLANs on to each other.
    pub split: Vec<(BTreeSet<String>, VlanSet)>,
    /// Where VLANs are passed on between server tunnels here: the VLANs and
    /// the groups of peers between which they pass.
    pub passes: Vec<(VlanSet, Vec<BTreeSet<String>>)>,
}

/// A tunnel of the network that carries VLANs.
struct Edge {
    a: String,
    b: String,
    servers: bool,
    vlans: VlanSet,
}

fn edges(network: &Network) -> Vec<Edge> {
    let online = |name: &str| network.site(name).is_some_and(|s| s.online);
    topology::edges(network)
        .into_iter()
        .filter(|(a, b, _)| online(a) && online(b))
        .filter_map(|(a, b, servers)| {
            let vlans = topology::tunnel_vlans(network, &a, &b)?;
            (!vlans.is_empty()).then_some(Edge {
                a,
                b,
                servers,
                vlans,
            })
        })
        .collect()
}

/// VLANs with the same tunnels, as (edge indices, VLANs).
fn classes(edges: &[Edge]) -> Vec<(Vec<usize>, VlanSet)> {
    let mut by_edges: BTreeMap<Vec<usize>, Vec<u16>> = BTreeMap::new();
    for vid in VID_MIN..=VID_MAX {
        let carrying: Vec<usize> = edges
            .iter()
            .enumerate()
            .filter(|(_, e)| e.vlans.contains(vid))
            .map(|(i, _)| i)
            .collect();
        if !carrying.is_empty() {
            by_edges.entry(carrying).or_default().push(vid);
        }
    }
    by_edges
        .into_iter()
        .map(|(edges, vids)| (edges, vids.into_iter().collect()))
        .collect()
}

/// The block of every edge of the graph (Hopcroft–Tarjan).
fn blocks(edges: &[Edge], members: &[usize]) -> BTreeMap<usize, usize> {
    let mut nodes: BTreeMap<&str, usize> = BTreeMap::new();
    for &i in members {
        for name in [edges[i].a.as_str(), edges[i].b.as_str()] {
            let next = nodes.len();
            nodes.entry(name).or_insert(next);
        }
    }
    let mut adjacent: Vec<Vec<(usize, usize)>> = vec![Vec::new(); nodes.len()];
    for &i in members {
        let (a, b) = (nodes[edges[i].a.as_str()], nodes[edges[i].b.as_str()]);
        adjacent[a].push((b, i));
        adjacent[b].push((a, i));
    }
    struct Search<'a> {
        adjacent: &'a [Vec<(usize, usize)>],
        depth: Vec<Option<usize>>,
        low: Vec<usize>,
        stack: Vec<usize>,
        block_of: BTreeMap<usize, usize>,
        blocks: usize,
    }
    impl Search<'_> {
        fn visit(&mut self, node: usize, via: Option<usize>, depth: usize) {
            self.depth[node] = Some(depth);
            self.low[node] = depth;
            for &(next, edge) in &self.adjacent[node] {
                if Some(edge) == via {
                    continue;
                }
                match self.depth[next] {
                    None => {
                        self.stack.push(edge);
                        self.visit(next, Some(edge), depth + 1);
                        self.low[node] = self.low[node].min(self.low[next]);
                        if self.low[next] >= depth {
                            // `node` separates the edges above: one block.
                            while let Some(e) = self.stack.pop() {
                                self.block_of.insert(e, self.blocks);
                                if e == edge {
                                    break;
                                }
                            }
                            self.blocks += 1;
                        }
                    }
                    Some(d) if d < depth => {
                        self.stack.push(edge);
                        self.low[node] = self.low[node].min(d);
                    }
                    Some(_) => {}
                }
            }
        }
    }
    let count = nodes.len();
    let mut search = Search {
        adjacent: &adjacent,
        depth: vec![None; count],
        low: vec![0; count],
        stack: Vec::new(),
        block_of: BTreeMap::new(),
        blocks: 0,
    };
    for node in 0..count {
        if search.depth[node].is_none() {
            search.visit(node, None, 0);
        }
    }
    search.block_of
}

/// The tunnels of `me` in one class, grouped by block: (peer, server tunnel).
fn groups_at(
    edges: &[Edge],
    members: &[usize],
    block_of: &BTreeMap<usize, usize>,
    me: &str,
) -> Vec<Vec<(String, bool)>> {
    let mut groups: BTreeMap<usize, Vec<(String, bool)>> = BTreeMap::new();
    for &i in members {
        let e = &edges[i];
        let peer = if e.a == me {
            &e.b
        } else if e.b == me {
            &e.a
        } else {
            continue;
        };
        groups
            .entry(block_of[&i])
            .or_default()
            .push((peer.clone(), e.servers));
    }
    groups.into_values().collect()
}

/// How the site `me` keeps the split horizon in `network`.
pub fn forwarding(network: &Network, me: &str) -> Forwarding {
    let kernel_only = || Forwarding {
        isolated: topology::tunnels_for(network, me)
            .iter()
            .filter(|t| t.isolated())
            .map(|t| t.peer.name.clone())
            .collect(),
        split: Vec::new(),
        passes: Vec::new(),
    };
    // Without limits per tunnel all servers carry the same VLANs to each
    // other: one block, the usual split horizon.
    if network.tunnels.is_empty() {
        return kernel_only();
    }
    let edges = edges(network);
    let mut kernel = true;
    let mut split: BTreeMap<BTreeSet<String>, VlanSet> = BTreeMap::new();
    let mut passes: Vec<(VlanSet, Vec<BTreeSet<String>>)> = Vec::new();
    for (members, vlans) in classes(&edges) {
        let block_of = blocks(&edges, &members);
        let groups = groups_at(&edges, &members, &block_of, me);
        let server_groups: Vec<BTreeSet<String>> = groups
            .iter()
            .map(|g| {
                g.iter()
                    .filter(|(_, servers)| *servers)
                    .map(|(p, _)| p.clone())
                    .collect::<BTreeSet<String>>()
            })
            .filter(|g| !g.is_empty())
            .collect();
        if server_groups.len() > 1 {
            // Server tunnels in different blocks: this site passes on.
            kernel = false;
            passes.push((vlans.clone(), server_groups));
        }
        for group in groups.iter().filter(|g| g.len() > 1) {
            let ports: BTreeSet<String> = group.iter().map(|(p, _)| p.clone()).collect();
            let entry = split.entry(ports).or_default();
            *entry = entry.iter().chain(vlans.iter()).collect();
        }
    }
    if kernel {
        return Forwarding {
            passes,
            ..kernel_only()
        };
    }
    Forwarding {
        isolated: BTreeSet::new(),
        split: split.into_iter().collect(),
        passes,
    }
}

/// Sites that take part in the same VLANs but do not reach each other,
/// because the tunnels carrying them form a ring without every direct
/// tunnel: (VLANs, the two sites, how many such pairs there are).
pub fn unreachable(network: &Network) -> Vec<(VlanSet, String, String, usize)> {
    let edges = edges(network);
    let mut out = Vec::new();
    for (members, vlans) in classes(&edges) {
        let block_of = blocks(&edges, &members);
        // A frame at `node` that came in through block `from` goes on
        // through every tunnel of another block.
        let mut sites: BTreeSet<&str> = BTreeSet::new();
        for &i in &members {
            sites.insert(&edges[i].a);
            sites.insert(&edges[i].b);
        }
        let mut pairs: Vec<(String, String)> = Vec::new();
        for &start in &sites {
            let mut reached: BTreeSet<&str> = BTreeSet::from([start]);
            let mut seen: BTreeSet<(&str, usize)> = BTreeSet::new();
            let mut queue: Vec<(&str, Option<usize>)> = vec![(start, None)];
            while let Some((node, from)) = queue.pop() {
                for &i in &members {
                    let e = &edges[i];
                    let next = if e.a == node {
                        e.b.as_str()
                    } else if e.b == node {
                        e.a.as_str()
                    } else {
                        continue;
                    };
                    let block = block_of[&i];
                    if from == Some(block) || !seen.insert((next, block)) {
                        continue;
                    }
                    reached.insert(next);
                    queue.push((next, Some(block)));
                }
            }
            for &other in &sites {
                if start < other && !reached.contains(other) {
                    pairs.push((start.to_string(), other.to_string()));
                }
            }
        }
        if let Some((a, b)) = pairs.first().cloned() {
            out.push((vlans, a, b, pairs.len()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;
    use crate::config::Site;

    fn site(name: &str, last: u8, uplink: Option<&str>) -> Site {
        Site {
            name: name.into(),
            address: Ipv4Addr::new(10, 0, 0, last),
            id: None,
            vlans: None,
            own_vlans: None,
            online: true,
            managed_by: None,
            self_managed: false,
            uplink: uplink.map(Into::into),
            breakout: Default::default(),
            ports: Default::default(),
        }
    }

    /// Four servers a, b, c, d and a client e of a; VLANs 10-20.
    fn network() -> Network {
        let mut network = Network::new(VlanSet::from_range(10, 20));
        network.sites = vec![
            site("a", 1, None),
            site("b", 2, None),
            site("c", 3, None),
            site("d", 4, None),
            site("e", 5, Some("a")),
        ];
        network
    }

    fn without(network: &mut Network, a: &str, b: &str, vid: u16) {
        let mut vlans = VlanSet::from_range(10, 20);
        vlans.remove(vid);
        network.set_tunnel_limit(a, b, Some(vlans));
    }

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    }

    #[test]
    fn without_limits_the_kernel_isolates_the_server_tunnels() {
        let network = network();
        for me in ["a", "b", "c", "d"] {
            let f = forwarding(&network, me);
            assert!(f.split.is_empty(), "{me}: {f:?}");
            assert!(f.passes.is_empty(), "{me}");
        }
        assert_eq!(forwarding(&network, "a").isolated, set(&["b", "c", "d"]));
        assert!(forwarding(&network, "e").isolated.is_empty(), "a client");
        assert!(unreachable(&network).is_empty());
    }

    #[test]
    fn a_chain_is_passed_on_by_the_site_in_the_middle() {
        // VLAN 15 only on a – c, b – c and c – d: a star around c.
        let mut network = network();
        for (x, y) in [("a", "b"), ("a", "d"), ("b", "d")] {
            without(&mut network, x, y, 15);
        }
        let c = forwarding(&network, "c");
        assert!(c.isolated.is_empty(), "nftables instead of isolated");
        assert_eq!(c.passes.len(), 1);
        assert_eq!(c.passes[0].0, VlanSet::from_range(15, 15));
        assert_eq!(c.passes[0].1.len(), 3, "a, b and d each in a block");
        // All other VLANs: the four servers in one block.
        assert!(
            c.split
                .iter()
                .any(|(ports, vlans)| *ports == set(&["a", "b", "d"]) && !vlans.contains(15)),
            "{:?}",
            c.split
        );
        assert!(
            !c.split.iter().any(|(_, vlans)| vlans.contains(15)),
            "VLAN 15 passes between all of them"
        );
        // a has only one server tunnel with VLAN 15: the kernel suffices.
        let a = forwarding(&network, "a");
        assert_eq!(a.isolated, set(&["b", "c", "d"]));
        assert!(
            unreachable(&network).is_empty(),
            "everyone reaches everyone"
        );
    }

    #[test]
    fn a_triangle_needs_no_passing_on() {
        let mut network = network();
        // VLAN 15 only between a, b and c.
        for (x, y) in [("a", "d"), ("b", "d"), ("c", "d")] {
            without(&mut network, x, y, 15);
        }
        for me in ["a", "b", "c", "d"] {
            assert!(forwarding(&network, me).passes.is_empty(), "{me}");
        }
        assert!(unreachable(&network).is_empty());
    }

    #[test]
    fn a_triangle_with_a_branch_passes_on_at_the_branch() {
        let mut network = network();
        // VLAN 15: triangle a, b, c and d only via c.
        for (x, y) in [("a", "d"), ("b", "d")] {
            without(&mut network, x, y, 15);
        }
        let c = forwarding(&network, "c");
        assert_eq!(c.passes.len(), 1);
        let groups = &c.passes[0].1;
        assert!(groups.contains(&set(&["a", "b"])) && groups.contains(&set(&["d"])));
        // Inside the triangle no passing on.
        assert!(
            c.split
                .iter()
                .any(|(ports, vlans)| *ports == set(&["a", "b"]) && vlans.contains(15))
        );
        assert!(unreachable(&network).is_empty());
    }

    #[test]
    fn a_ring_is_not_passed_on_and_warned_about() {
        let mut network = network();
        // VLAN 15 on the ring a – b – c – d – a only.
        for (x, y) in [("a", "c"), ("b", "d")] {
            without(&mut network, x, y, 15);
        }
        for me in ["a", "b", "c", "d"] {
            assert!(forwarding(&network, me).passes.is_empty(), "{me}");
        }
        let gaps = unreachable(&network);
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        let (vlans, a, b, count) = &gaps[0];
        assert_eq!(*vlans, VlanSet::from_range(15, 15));
        assert_eq!((a.as_str(), b.as_str()), ("a", "c"));
        // a – c and b – d, and the client e (behind a) does not reach c.
        assert_eq!(*count, 3);
    }

    #[test]
    fn offline_sites_do_not_count() {
        let mut network = network();
        for (x, y) in [("a", "b"), ("a", "d"), ("b", "d")] {
            without(&mut network, x, y, 15);
        }
        network.sites[2].online = false; // c
        // Nobody passes on; a, b and d no longer reach each other in VLAN 15
        // (only through c), but that is what offline means.
        for me in ["a", "b", "d"] {
            assert!(forwarding(&network, me).passes.is_empty(), "{me}");
        }
    }
}
