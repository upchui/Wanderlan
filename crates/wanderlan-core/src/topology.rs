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
use crate::identity::{IFNAME_PART_LEN, SiteId};

/// Prefix of the tunnel (VXLAN) interfaces of the default network towards
/// a peer without an identity (`vx-<name>`).
pub const TUNNEL_PREFIX: &str = "vx-";
/// Prefix of the GRETAP interfaces of older versions (removed on upgrade).
pub const LEGACY_TUNNEL_PREFIX: &str = "gt-";
/// The tag of the default network in interface names (`vx…`); every other
/// network of a box has a tag of its own (a lowercase letter or a digit).
pub const DEFAULT_TAG: char = 'x';

/// True for a tag of a network in interface names.
pub fn is_tag(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit()
}

/// The tag of the network whose tunnel interface `name` is, if it is one:
/// `v`, the tag and the start of an identity (13 characters of base32);
/// `vx-<name>` towards a peer of an older version in the default network.
/// Other names that start with `v` (`ve-foo` of systemd-nspawn, `vlan10`)
/// are not wanderlan's.
pub fn tunnel_tag(name: &str) -> Option<char> {
    if name
        .strip_prefix(TUNNEL_PREFIX)
        .is_some_and(|n| !n.is_empty())
    {
        return Some(DEFAULT_TAG);
    }
    let rest = name.strip_prefix('v')?;
    let tag = rest.chars().next().filter(|c| is_tag(*c))?;
    let rest = &rest[1..];
    let by_id = rest.len() == IFNAME_PART_LEN
        && rest
            .bytes()
            .all(|c| c.is_ascii_lowercase() || (b'2'..=b'7').contains(&c));
    by_id.then_some(tag)
}

/// True for the name of a tunnel interface of wanderlan, of any network
/// (also of an older version).
pub fn is_tunnel_name(name: &str) -> bool {
    name.starts_with(LEGACY_TUNNEL_PREFIX) || tunnel_tag(name).is_some()
}

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
    /// Name of the tunnel interface in the network with `tag`.
    pub fn ifname(&self, tag: char) -> String {
        tunnel_name(tag, self.peer)
    }

    /// Server-to-server ports are isolated (split horizon); the ports of a
    /// client and its uplink are ordinary bridge ports.
    pub fn isolated(&self) -> bool {
        self.kind == TunnelKind::Server
    }
}

/// Name of the tunnel interface towards a peer in the network with `tag`:
/// `v`, the tag and the start of the peer's identity, so that it stays when
/// the peer is renamed and two peers with the same name get their own. In
/// the default network a peer of an older version without an identity
/// keeps `vx-<name>`; in a further network such a peer is named by its
/// placeholder (which follows its name).
pub fn tunnel_name(tag: char, peer: &Site) -> String {
    if peer.has_identity() || tag != DEFAULT_TAG {
        format!("v{tag}{}", peer.id.ifname_part())
    } else {
        format!("{TUNNEL_PREFIX}{}", peer.name)
    }
}

/// The peer the tunnel interface `ifname` of the network with `tag` leads
/// to.
pub fn peer_of<'a>(network: &'a Network, tag: char, ifname: &str) -> Option<&'a Site> {
    network.sites.iter().find(|s| tunnel_name(tag, s) == ifname)
}

/// True for a server (a site without uplink).
pub fn is_server(site: &Site) -> bool {
    site.uplink.is_none()
}

/// The tunnels the given site has to build, sorted by peer name (then by
/// identity): to its uplink, to its clients and, as a server, to every other
/// server. A site that is not part of the network has none.
pub fn tunnels_for(network: &Network, local: SiteId) -> Vec<Tunnel<'_>> {
    let Some(me) = network.site(local) else {
        return Vec::new();
    };
    let mut out: Vec<Tunnel<'_>> = network
        .sites
        .iter()
        .filter(|peer| peer.id != local)
        .filter_map(|peer| {
            let kind = if me.uplink == Some(peer.id) {
                TunnelKind::Uplink
            } else if peer.uplink == Some(local) {
                TunnelKind::Client
            } else if is_server(me) && is_server(peer) {
                TunnelKind::Server
            } else {
                return None;
            };
            Some(Tunnel { peer, kind })
        })
        .collect();
    out.sort_by(|a, b| (&a.peer.name, a.peer.id).cmp(&(&b.peer.name, b.peer.id)));
    out
}

/// The VLANs the tunnel between `a` and `b` carries, or `None` if there is
/// no tunnel between them. Between servers: the VLANs both take part in.
/// Between a site and its client: also the own VLANs of the upper site,
/// never those of the client. Never more than the tunnel's own limit
/// (`[[tunnel]]`).
pub fn tunnel_vlans(network: &Network, a: SiteId, b: SiteId) -> Option<VlanSet> {
    let me = network.site(a)?;
    let tunnel = tunnels_for(network, a)
        .into_iter()
        .find(|t| t.peer.id == b)?;
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

/// All tunnels of the network as unordered site pairs (smaller identity
/// first), with `true` for tunnels between servers.
pub fn edges(network: &Network) -> BTreeSet<(SiteId, SiteId, bool)> {
    let mut out = BTreeSet::new();
    for site in &network.sites {
        for t in tunnels_for(network, site.id) {
            let (a, b) = if site.id <= t.peer.id {
                (site.id, t.peer.id)
            } else {
                (t.peer.id, site.id)
            };
            out.insert((a, b, t.kind == TunnelKind::Server));
        }
    }
    out
}

/// The chain from `site` up to its server: the site, its uplink, the uplink
/// of that, ... Stops at a loop or an unknown site (invalid configurations).
fn ancestors(network: &Network, site: SiteId) -> Vec<SiteId> {
    let mut chain = vec![site];
    let mut current = site;
    while let Some(up) = network.site(current).and_then(|s| s.uplink) {
        if chain.contains(&up) || network.site(up).is_none() {
            break;
        }
        chain.push(up);
        current = up;
    }
    chain
}

/// True if `site` hangs (directly or indirectly) off `root`: its chain of
/// servers leads there. A site is not below itself.
pub fn is_below(network: &Network, site: SiteId, root: SiteId) -> bool {
    site != root && ancestors(network, site).contains(&root)
}

/// The sites below `root`: its clients, their clients, ... in the order of
/// the network.
pub fn below(network: &Network, root: SiteId) -> Vec<SiteId> {
    network
        .sites
        .iter()
        .map(|s| s.id)
        .filter(|s| is_below(network, *s, root))
        .collect()
}

/// A site that `new` moves away from under the client it hangs off in `old`,
/// to a place outside that client's sites, as (site, that client). Only its
/// server tells a site where it went, and that client would no longer know
/// it (a server knows the whole network, so moving a site away from a server
/// is fine).
pub fn stranded(old: &Network, new: &Network) -> Option<(SiteId, SiteId)> {
    old.sites.iter().find_map(|site| {
        let up = site.uplink?;
        let up_was_client = old.site(up)?.uplink.is_some();
        new.site(site.id)?;
        let known = new
            .site(up)
            .is_some_and(|u| u.uplink.is_none() || is_below(new, site.id, up));
        (up_was_client && !known).then_some((site.id, up))
    })
}

/// True if a site has clients of its own.
pub fn has_clients(network: &Network, site: SiteId) -> bool {
    network.sites.iter().any(|s| s.uplink == Some(site))
}

/// The role of a site for displays: `server`, `client of hq`, or
/// `client of hq + server` for a client with clients of its own (a client in
/// its tunnel to its server, the server in the tunnels to its clients).
pub fn role(uplink: Option<&str>, has_clients: bool) -> String {
    match uplink {
        None => "server".to_string(),
        Some(up) if has_clients => format!("client of {up} + server"),
        Some(up) => format!("client of {up}"),
    }
}

/// The neighbour through which `from` reaches `to`, if `to` is neither
/// `from` itself nor a direct neighbour: shown as "via …".
pub fn via(network: &Network, from: SiteId, to: SiteId) -> Option<SiteId> {
    network.site(from)?;
    network.site(to)?;
    if from == to {
        return None;
    }
    let up_from_target = ancestors(network, to);
    let next = match up_from_target.iter().position(|s| *s == from) {
        // `to` hangs (indirectly) off `from`: towards that client.
        Some(pos) => *up_from_target.get(pos.checked_sub(1)?)?,
        None => match network.site(from).and_then(|s| s.uplink) {
            // Up towards the own server.
            Some(up) => up,
            // A server: to the server `to` hangs off.
            None => *up_from_target.last()?,
        },
    };
    (next != to).then_some(next)
}

/// Display order of sites given as `(key, uplink)`: every server (or site
/// whose uplink is unknown here), each followed by its clients, recursively.
/// Returns `(index, depth)`; sites in an uplink loop come last at depth 0.
pub fn tree_order_by<K: PartialEq + Copy>(sites: &[(K, Option<K>)]) -> Vec<(usize, usize)> {
    fn add<K: PartialEq + Copy>(
        sites: &[(K, Option<K>)],
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
        let key = sites.get(index).map(|s| s.0);
        for (child, (_, uplink)) in sites.iter().enumerate() {
            if uplink.is_some() && *uplink == key {
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
    let sites: Vec<(SiteId, Option<SiteId>)> =
        network.sites.iter().map(|s| (s.id, s.uplink)).collect();
    tree_order_by(&sites)
}

/// Everything about the network that determines over which ports frames
/// travel: the tunnels and whether they connect servers. Two networks with the
/// same key have the same layer-2 paths, so learned MAC addresses stay
/// valid; VLAN changes do not move addresses between ports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardingKey {
    edges: BTreeSet<(SiteId, SiteId, bool)>,
    /// The tunnel limits: they decide where VLANs are passed on.
    limits: Vec<([SiteId; 2], String)>,
}

/// Computes the [`ForwardingKey`] of a network.
pub fn forwarding_key(network: &Network) -> ForwardingKey {
    let mut limits: Vec<([SiteId; 2], String)> = network
        .tunnels
        .iter()
        .map(|t| (t.sites, t.vlans.to_string()))
        .collect();
    limits.sort();
    ForwardingKey {
        edges: edges(network),
        limits,
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;
    use crate::config::{VlanSet, site_pair};

    fn id(name: &str) -> SiteId {
        SiteId::provisional(name)
    }

    fn site(name: &str, last: u8) -> Site {
        Site::new(name, Ipv4Addr::new(10, 0, 0, last))
    }

    fn client(name: &str, last: u8, uplink: &str) -> Site {
        Site {
            uplink: Some(id(uplink)),
            ..site(name, last)
        }
    }

    fn edge(a: &str, b: &str, server: bool) -> (SiteId, SiteId, bool) {
        let [a, b] = site_pair(id(a), id(b));
        (a, b, server)
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
        tunnels_for(network, id(site))
            .iter()
            .map(|t| (t.peer.name.clone(), t.kind))
            .collect()
    }

    #[test]
    fn servers_have_a_tunnel_to_every_other_server() {
        let network = example();
        let names: Vec<String> = tunnels_for(&network, id("heim"))
            .iter()
            .map(|t| t.ifname(DEFAULT_TAG))
            .collect();
        assert_eq!(names, vec!["vx-buero", "vx-haus2"], "known by their names");
        assert!(
            tunnels_for(&network, id("heim"))
                .iter()
                .all(Tunnel::isolated)
        );
        assert_eq!(tunnels_for(&network, id("haus2")).len(), 2);
        assert!(tunnels_for(&network, id("unknown")).is_empty());
        // With an identity: named by it, whatever the site is called.
        let mut with_id = example();
        with_id.sites[1].id = SiteId::from_bytes([0; 32]);
        let tunnels = tunnels_for(&with_id, id("heim"));
        assert_eq!(tunnels[0].ifname(DEFAULT_TAG), "vxaaaaaaaaaaaaa");
        assert_eq!(tunnels[0].ifname('a'), "vaaaaaaaaaaaaaa");
        assert!(is_tunnel_name("vxaaaaaaaaaaaaa"));
        assert_eq!(tunnel_tag("v3aaaaaaaaaaaaa"), Some('3'));
        assert_eq!(tunnel_tag("vx-lab"), Some(DEFAULT_TAG));
        assert!(!is_tunnel_name("vxaaaaaaaaaaaa"));
        assert!(!is_tunnel_name("veth1a2b3c4"));
        // systemd-nspawn and others: not wanderlan's.
        assert!(!is_tunnel_name("ve-foo"));
        assert!(!is_tunnel_name("vb-lab"));
        assert_eq!(
            peer_of(&with_id, DEFAULT_TAG, "vxaaaaaaaaaaaaa").map(|s| s.name.as_str()),
            Some("buero")
        );
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
            !tunnels_for(&network, id("marcel"))
                .iter()
                .any(Tunnel::isolated),
            "client ports are ordinary ports"
        );
    }

    #[test]
    fn via_names_the_next_hop() {
        let network = with_clients();
        let via = |a: &str, b: &str| via(&network, id(a), id(b));
        assert_eq!(via("marcel", "site2"), Some(id("alex")));
        assert_eq!(via("marcel", "dc"), Some(id("alex")));
        assert_eq!(via("marcel", "alex"), None, "direct");
        assert_eq!(via("alex", "x"), Some(id("marcel")));
        assert_eq!(via("dc", "x"), Some(id("alex")));
        assert_eq!(via("dc", "site2"), Some(id("alex")));
        assert_eq!(via("x", "site2"), Some(id("marcel")));
        assert_eq!(via("x", "x"), None);
        assert_eq!(via("x", "nowhere"), None);
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
        assert!(e.contains(&edge("buero", "heim", true)));
        assert!(e.contains(&edge("buero", "haus2", true)));
        assert!(e.contains(&edge("haus2", "heim", true)));
        let e = edges(&with_clients());
        assert_eq!(e.len(), 4);
        assert!(e.contains(&edge("alex", "marcel", false)));
        assert!(!e.contains(&edge("marcel", "site2", false)));
    }

    #[test]
    fn a_single_site_has_no_tunnels() {
        let mut network = Network::new(VlanSet::from_range(10, 50));
        network.sites = vec![site("first", 1)];
        assert!(tunnels_for(&network, id("first")).is_empty());
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
        c.sites[2].uplink = Some(id("heim"));
        assert_ne!(forwarding_key(&a), forwarding_key(&c));
    }

    #[test]
    fn a_tunnel_carries_at_most_its_limit() {
        let mut network = with_clients();
        // dc takes part in 30-60.
        network.sites[1].vlans = Some(VlanSet::from_range(30, 60));
        assert_eq!(
            tunnel_vlans(&network, id("alex"), id("dc")),
            Some(VlanSet::from_range(30, 50)),
            "servers: what both have"
        );
        // Down to a client also the own VLANs of the upper site.
        network.sites[0].own_vlans = Some(VlanSet::from_range(60, 60));
        network.sites[2].vlans = Some(VlanSet::from_range(10, 60));
        assert!(
            tunnel_vlans(&network, id("alex"), id("marcel"))
                .unwrap()
                .contains(60)
        );
        assert_eq!(
            tunnel_vlans(&network, id("marcel"), id("alex")),
            tunnel_vlans(&network, id("alex"), id("marcel")),
            "the same from both ends"
        );
        // The limit narrows it, entered in either order.
        network.set_tunnel_limit(id("dc"), id("alex"), Some(VlanSet::from_range(40, 45)));
        assert_eq!(
            tunnel_vlans(&network, id("alex"), id("dc")),
            Some(VlanSet::from_range(40, 45))
        );
        network.set_tunnel_limit(id("alex"), id("dc"), Some(VlanSet::new()));
        assert_eq!(
            tunnel_vlans(&network, id("dc"), id("alex")),
            Some(VlanSet::new())
        );
        assert_eq!(network.tunnels.len(), 1);
        assert_eq!(network.tunnels[0].sites, site_pair(id("alex"), id("dc")));
        // Two clients of one site have no tunnel.
        assert_eq!(tunnel_vlans(&network, id("marcel"), id("site2")), None);
        network.set_tunnel_limit(id("alex"), id("dc"), None);
        assert!(network.tunnels.is_empty());
    }
}
