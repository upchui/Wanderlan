//! What each site knows of the network.
//!
//! Servers (sites without uplink) know the whole network. A client knows
//! only itself, the site it is plugged into (its uplink, which it sees as a
//! server) and the sites below it (its clients, their clients, ...); never
//! the clients of other sites. Its `network.toml` contains exactly that.
//!
//! A client's changes reach the network through its uplink: the uplink
//! fetches the client's view and merges what the client is responsible for
//! (itself, the sites below it and the network settings) into its own
//! network. A client with clients of its own passes their changes on the
//! same way, so they travel up through every site on the way.

use std::collections::BTreeSet;

use crate::config::{
    MAX_REVISION, Network, Site, TunnelVlans, VlanMap, fingerprint, next_revision, validate,
};
use crate::identity::SiteId;
use crate::{policy, topology};

/// How far a client's view may count the revision on past its uplink's.
use crate::config::MAX_REVISION_STEP;

/// The part of `network` the site `site` knows: everything for a server; for a
/// client itself, its uplink (without the uplink's own uplink) and the sites
/// below it. An unknown site knows no sites at all.
pub fn view_for(network: &Network, site: SiteId) -> Network {
    let Some(me) = network.site(site) else {
        return Network {
            sites: Vec::new(),
            maps: Vec::new(),
            tunnels: Vec::new(),
            ..network.clone()
        };
    };
    let Some(uplink) = me.uplink else {
        return network.clone();
    };
    let sites: Vec<Site> = network
        .sites
        .iter()
        .filter(|s| s.id == site || s.id == uplink || topology::is_below(network, s.id, site))
        .map(|s| {
            let mut s = s.clone();
            if s.id == uplink {
                // Seen from here, the uplink is a server; its breakout (as a
                // client + server, towards its own server) is its own matter.
                s.uplink = None;
                s.breakout.clear();
                s.changes = crate::config::Changes::default();
            }
            s
        })
        .collect();
    let maps = network
        .maps
        .iter()
        .filter(|m| sites.iter().any(|s| s.id == m.site))
        .cloned()
        .collect();
    let tunnels = network
        .tunnels
        .iter()
        .filter(|t| t.sites.iter().all(|id| sites.iter().any(|s| s.id == *id)))
        .cloned()
        .collect();
    Network {
        sites,
        maps,
        tunnels,
        ..network.clone()
    }
}

fn maps_of(network: &Network, site: SiteId) -> Vec<&VlanMap> {
    let mut maps: Vec<&VlanMap> = network.maps_of(site).collect();
    maps.sort_by_key(|m| (m.local, m.shared));
    maps
}

/// What [`merge`] took over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merged {
    /// The network with the client's changes.
    pub network: Network,
    /// Sites of the client that got another identity (itself, its own
    /// clients: a claim, a box that joined again), as (old, new).
    pub rekeyed: Vec<(SiteId, SiteId)>,
}

/// Merges the view of the client `from` into `full` (the network of its
/// uplink): its own entry, the entries of the sites below it (new ones too,
/// removed ones go), their mappings and the network settings. Anything else
/// in the view must be as `from` received it. A site of the view that got
/// another identity (it claimed its own, a box joined again) keeps it in the
/// whole network. The result is checked against the rights of `from` and of
/// the sites below it (their changes come through it) and validated.
pub fn merge(full: &Network, view: &Network, from: SiteId) -> Result<Merged, String> {
    let (_, rekeyed) = crate::rename::rekeyed(&view_for(full, from), view);
    let mut base = full.clone();
    for (old, new) in &rekeyed {
        base.rekey(*old, *new);
        // A site of an older version renamed: its name comes with it.
        if let (Some(now), Some(entry)) = (view.site(*new), base.site_mut(*new))
            && !now.has_identity()
        {
            entry.name.clone_from(&now.name);
        }
    }
    let from_now = rekeyed
        .iter()
        .find(|(old, _)| *old == from)
        .map_or(from, |(_, new)| *new);
    // A client counts on from the revision it has; one far ahead is no
    // honest client (it would end the revisions for the whole network).
    if view.revision > full.revision.saturating_add(MAX_REVISION_STEP) {
        return Err(format!(
            "its revision {} is too far ahead of {}",
            view.revision, full.revision
        ));
    }
    let mut merged = merge_named(&base, view, from_now)?;
    merged.revision = if fingerprint(&merged) == fingerprint(full) {
        full.revision.max(view.revision)
    } else {
        view.revision.max(next_revision(full.revision))
    };
    // The client gets back another view than it sent (it lacked a change
    // made here): with a revision above its own, else it keeps what it sent.
    if fingerprint(&view_for(&merged, from_now)) != fingerprint(view) {
        merged.revision = merged.revision.max(next_revision(view.revision));
    }
    merged.revision = merged.revision.min(MAX_REVISION);
    policy::check_relayed(full, &merged, from)?;
    if let Some(error) = validate(&merged, None).into_iter().find(|d| d.is_error()) {
        return Err(error.message);
    }
    Ok(Merged {
        network: merged,
        rekeyed,
    })
}

/// [`merge`] after the new identities: `full` already has them.
fn merge_named(full: &Network, view: &Network, from: SiteId) -> Result<Network, String> {
    let me = view.label(from);
    if view.site(from).is_none() {
        return Err(format!(
            "the configuration of \"{me}\" does not contain \"{me}\" itself"
        ));
    }
    // `from` and the sites below it, each in the network it belongs to.
    let owned_in = |n: &Network, s: &Site| s.id == from || topology::is_below(n, s.id, from);
    let owned_view = |s: &Site| owned_in(view, s);
    let owned_full = |s: &Site| owned_in(full, s);
    let expected = view_for(full, from);
    for site in &view.sites {
        if owned_view(site) {
            if full.site(site.id).is_some_and(|f| !owned_full(f)) {
                return Err(format!(
                    "\"{me}\" may only change itself and the sites below it, not \"{}\"",
                    full.label(site.id)
                ));
            }
            continue;
        }
        let same = expected.site(site.id) == Some(site)
            && maps_of(view, site.id) == maps_of(&expected, site.id);
        if !same {
            return Err(format!(
                "\"{me}\" may only change itself and the sites below it, not \"{}\"",
                view.label(site.id)
            ));
        }
    }
    if let Some(missing) = expected
        .sites
        .iter()
        .find(|s| !owned_full(s) && view.site(s.id).is_none())
    {
        return Err(format!(
            "\"{me}\" may only change itself and the sites below it, not remove \"{}\"",
            full.label(missing.id)
        ));
    }

    let mut merged = full.clone();
    merged.vlans = view.vlans.clone();
    merged.control_port = view.control_port;
    merged.tunnel_port = view.tunnel_port;
    merged.mtu_probe_interval = view.mtu_probe_interval;
    merged.commit_timeout = view.commit_timeout;
    merged.mss = view.mss;
    // Its own sites as in the view (missing ones are removed), in place.
    merged.sites = full
        .sites
        .iter()
        .filter_map(|s| {
            if owned_full(s) {
                view.site(s.id).filter(|v| owned_view(v)).cloned()
            } else {
                Some(s.clone())
            }
        })
        .collect();
    for site in view
        .sites
        .iter()
        .filter(|s| owned_view(s) && full.site(s.id).is_none())
    {
        merged.sites.push(site.clone());
    }
    let ids: BTreeSet<SiteId> = full
        .sites
        .iter()
        .filter(|s| owned_full(s))
        .chain(view.sites.iter().filter(|s| owned_view(s)))
        .map(|s| s.id)
        .collect();
    // Its own mappings as in the view; unchanged ones keep their place.
    let own_maps: Vec<&VlanMap> = view
        .maps
        .iter()
        .filter(|m| ids.contains(&m.site) && view.site(m.site).is_some_and(owned_view))
        .collect();
    merged
        .maps
        .retain(|m| !ids.contains(&m.site) || own_maps.contains(&m));
    for map in own_maps {
        if !merged.maps.contains(map) {
            merged.maps.push(map.clone());
        }
    }

    // The VLANs of its own tunnels as in the view; the others as received.
    // Its own are those with both ends in its view: a limit towards a site
    // it does not see (left over from where it hung before) stays.
    let own_tunnel = |t: &TunnelVlans| {
        t.sites.iter().any(|s| ids.contains(s))
            && t.sites
                .iter()
                .all(|s| view.site(*s).is_some() || ids.contains(s))
    };
    let foreign = |n: &Network| -> Vec<TunnelVlans> {
        let mut list: Vec<TunnelVlans> = n
            .tunnels
            .iter()
            .filter(|t| !own_tunnel(t))
            .cloned()
            .collect();
        list.sort_by_key(|t| t.sites);
        list
    };
    if foreign(view) != foreign(&expected) {
        return Err(format!(
            "\"{me}\" may only change the VLANs of its own tunnels"
        ));
    }
    merged
        .tunnels
        .retain(|t| !own_tunnel(t) || view.tunnels.contains(t));
    for tunnel in view.tunnels.iter().filter(|t| own_tunnel(t)) {
        if !merged.tunnels.contains(tunnel) {
            merged.tunnels.push(tunnel.clone());
        }
    }
    // A limit towards a site it removed goes with it.
    let sites: BTreeSet<SiteId> = merged.sites.iter().map(|s| s.id).collect();
    merged
        .tunnels
        .retain(|t| t.sites.iter().all(|s| sites.contains(s)));

    Ok(merged)
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;
    use crate::config::{Changes, LocalVid, VlanSet};

    fn id(name: &str) -> SiteId {
        SiteId::provisional(name)
    }

    fn site(name: &str, last: u8, uplink: Option<&str>) -> Site {
        Site {
            changes: Changes::Both,
            uplink: uplink.map(id),
            ..Site::new(name, Ipv4Addr::new(10, 0, 0, last))
        }
    }

    /// alex and dc are servers; marcel and site2 are clients of alex, x is a
    /// client of marcel, y a client of dc, w a client of x (so marcel and x
    /// are client + server).
    fn full() -> Network {
        let mut network = Network::new(VlanSet::from_range(10, 50));
        network.revision = 7;
        network.sites = vec![
            site("alex", 1, None),
            site("dc", 2, None),
            site("marcel", 3, Some("alex")),
            site("site2", 4, Some("alex")),
            site("x", 5, Some("marcel")),
            site("y", 6, Some("dc")),
            site("w", 7, Some("x")),
        ];
        network.maps = vec![
            VlanMap {
                site: id("site2"),
                local: LocalVid::Native,
                shared: 13,
            },
            VlanMap {
                site: id("marcel"),
                local: LocalVid::Native,
                shared: 14,
            },
        ];
        network
    }

    fn names(network: &Network) -> Vec<&str> {
        network.sites.iter().map(|s| s.name.as_str()).collect()
    }

    #[test]
    fn a_client_knows_its_server_and_the_sites_below_it_only() {
        let network = full();
        assert_eq!(
            names(&view_for(&network, id("alex"))),
            names(&network),
            "a server knows all"
        );
        let site2 = view_for(&network, id("site2"));
        assert_eq!(names(&site2), ["alex", "site2"]);
        assert_eq!(site2.maps.len(), 1, "only its own mapping");
        assert_eq!(site2.revision, 7);
        let marcel = view_for(&network, id("marcel"));
        assert_eq!(names(&marcel), ["alex", "marcel", "x", "w"]);
        let x = view_for(&network, id("x"));
        assert_eq!(names(&x), ["marcel", "x", "w"]);
        assert_eq!(
            x.by_name("marcel").unwrap().uplink,
            None,
            "its uplink is its server"
        );
        assert!(view_for(&network, id("nobody")).sites.is_empty());
        // Every view is a valid configuration of its own.
        for s in ["site2", "marcel", "x", "y", "w"] {
            let v = view_for(&network, id(s));
            assert!(!validate(&v, None).iter().any(|d| d.is_error()), "{s}");
        }
    }

    #[test]
    fn the_uplink_merges_what_the_client_is_responsible_for() {
        let network = full();
        let mut view = view_for(&network, id("marcel"));
        view.revision = 8;
        // marcel changes its client x and invites z.
        view.sites
            .iter_mut()
            .find(|s| s.name == "x")
            .unwrap()
            .online = false;
        view.sites.push(site("z", 9, Some("marcel")));
        let merged = merge(&network, &view, id("marcel")).unwrap().network;
        assert_eq!(merged.revision, 8);
        assert!(!merged.by_name("x").unwrap().online);
        assert!(merged.by_name("z").is_some());
        assert!(
            merged.by_name("y").is_some() && merged.by_name("site2").is_some(),
            "the rest stays"
        );
        assert_eq!(
            merged.by_name("alex").unwrap(),
            network.by_name("alex").unwrap()
        );

        // Removing its own client.
        let mut view = view_for(&network, id("marcel"));
        view.revision = 8;
        view.sites.retain(|s| s.name != "x");
        let merged = merge(&network, &view, id("marcel")).unwrap().network;
        assert!(merged.by_name("x").is_none());

        // An older view with changes still gets a newer revision.
        let mut view = view_for(&network, id("site2"));
        view.sites
            .iter_mut()
            .find(|s| s.name == "site2")
            .unwrap()
            .vlans = Some(VlanSet::from_range(13, 13));
        let merged = merge(&network, &view, id("site2")).unwrap().network;
        assert_eq!(merged.revision, 8);
        // Nothing changed: the revision is not raised.
        let same = merge(&network, &view_for(&network, id("site2")), id("site2"))
            .unwrap()
            .network;
        assert_eq!(same, network);

        // Its own ports come with the view; the settings of the network
        // (the defaults of every site) it does not change.
        let mut view = view_for(&network, id("site2"));
        view.sites
            .iter_mut()
            .find(|s| s.name == "site2")
            .unwrap()
            .ports
            .tunnel = Some(51900);
        let merged = merge(&network, &view, id("site2")).unwrap().network;
        assert_eq!(merged.tunnel_port_of(id("site2")), 51900);
        assert!(merged.revision > network.revision);
        let mut view = view_for(&network, id("site2"));
        view.tunnel_port = Some(51900);
        assert!(merge(&network, &view, id("site2")).is_err());
        // A revision far ahead would end the revisions of the network.
        let mut view = view_for(&network, id("site2"));
        view.revision = crate::config::MAX_REVISION;
        assert!(merge(&network, &view, id("site2")).is_err());
    }

    #[test]
    fn changes_travel_up_through_every_site_on_the_way() {
        let network = full();
        // x (a client of marcel) invites z and sets its own ports.
        let mut at_x = view_for(&network, id("x"));
        at_x.revision = 8;
        // z joined x with ports of its own.
        let mut z = site("z", 9, Some("x"));
        z.ports.control = Some(52100);
        at_x.sites.push(z);
        at_x.sites
            .iter_mut()
            .find(|s| s.name == "x")
            .unwrap()
            .ports
            .control = Some(52000);
        // marcel takes it over from x, alex from marcel.
        let at_marcel = merge(&view_for(&network, id("marcel")), &at_x, id("x"))
            .unwrap()
            .network;
        assert!(at_marcel.by_name("z").is_some());
        let at_alex = merge(&network, &at_marcel, id("marcel")).unwrap().network;
        assert_eq!(at_alex.by_name("z").unwrap().uplink, Some(id("x")));
        assert_eq!(at_alex.control_port_of(id("x")), 52000);
        assert_eq!(at_alex.control_port_of(id("z")), 52100);
        // And down again: x knows z.
        assert_eq!(
            names(&view_for(&at_alex, id("x"))),
            ["marcel", "x", "w", "z"]
        );
        // marcel may not set the ports of x itself, nor change its server.
        let mut own = view_for(&network, id("marcel"));
        own.sites
            .iter_mut()
            .find(|s| s.name == "x")
            .unwrap()
            .ports
            .control = Some(52000);
        let err =
            policy::check_edit(&view_for(&network, id("marcel")), &own, id("marcel")).unwrap_err();
        assert!(err.contains("only \"x\" itself"), "{err}");
        let mut up = view_for(&network, id("x"));
        up.sites
            .iter_mut()
            .find(|s| s.name == "marcel")
            .unwrap()
            .online = false;
        assert!(merge(&view_for(&network, id("marcel")), &up, id("x")).is_err());
    }

    #[test]
    fn a_site_with_tunnel_limits_is_removed_with_them() {
        let mut network = full();
        network.set_tunnel_limit(id("marcel"), id("x"), Some(VlanSet::from_range(10, 20)));
        network.set_tunnel_limit(id("dc"), id("x"), Some(VlanSet::from_range(10, 30)));
        // marcel removes x (and w below it).
        let mut view = view_for(&network, id("marcel"));
        view.revision = 8;
        view.sites.retain(|s| s.name != "x" && s.name != "w");
        view.tunnels.retain(|t| !t.touches(id("x")));
        let merged = merge(&network, &view, id("marcel")).unwrap().network;
        assert!(merged.by_name("x").is_none());
        assert!(merged.tunnel_limit(id("marcel"), id("x")).is_none());
        assert!(
            merged.tunnel_limit(id("dc"), id("x")).is_none(),
            "towards a site it did not see"
        );
    }

    #[test]
    fn what_a_client_does_not_see_stays_as_it_is() {
        let mut network = full();
        // A limit left over towards dc (x hung off it before): x's client
        // marcel does not see dc.
        network.set_tunnel_limit(id("dc"), id("x"), Some(VlanSet::from_range(10, 20)));
        let mut view = view_for(&network, id("marcel"));
        view.revision = 8;
        view.sites
            .iter_mut()
            .find(|s| s.name == "x")
            .unwrap()
            .online = false;
        let merged = merge(&network, &view, id("marcel")).unwrap().network;
        assert_eq!(
            merged.tunnel_limit(id("dc"), id("x")),
            Some(&VlanSet::from_range(10, 20))
        );
        // marcel is restricted, x released below it: the VLANs of the
        // network still need marcel's rights.
        let site = |n: &mut Network, name: &str| -> usize {
            n.sites.iter().position(|s| s.name == name).unwrap()
        };
        let m = site(&mut network, "marcel");
        network.sites[m].changes = Changes::Server;
        let mut view = view_for(&network, id("marcel"));
        view.revision = 8;
        view.vlans = VlanSet::from_range(10, 40);
        let err = merge(&network, &view, id("marcel")).unwrap_err();
        assert!(err.contains("VLANs of the network"), "{err}");
    }

    #[test]
    fn a_client_cannot_change_its_server_or_take_other_names() {
        let network = full();
        let mut view = view_for(&network, id("site2"));
        view.revision = 8;
        view.sites
            .iter_mut()
            .find(|s| s.name == "alex")
            .unwrap()
            .online = false;
        let err = merge(&network, &view, id("site2")).unwrap_err();
        assert!(
            err.contains("may only change itself and the sites below it"),
            "{err}"
        );

        let mut view = view_for(&network, id("site2"));
        view.sites.push(site("marcel", 20, Some("site2")));
        let err = merge(&network, &view, id("site2")).unwrap_err();
        assert!(err.contains("not \"marcel\""), "{err}");

        let mut view = view_for(&network, id("site2"));
        view.sites.retain(|s| s.name != "alex");
        assert!(merge(&network, &view, id("site2")).is_err());
        assert!(
            merge(&network, &view_for(&network, id("alex")), id("site2")).is_err(),
            "wrong sender"
        );
    }

    #[test]
    fn the_rights_of_the_client_apply() {
        let mut network = full();
        // Only alex, its server, changes the tunnel of site2.
        network
            .sites
            .iter_mut()
            .find(|s| s.name == "site2")
            .unwrap()
            .changes = Changes::Server;
        let mut view = view_for(&network, id("site2"));
        view.revision = 8;
        view.sites
            .iter_mut()
            .find(|s| s.name == "site2")
            .unwrap()
            .online = false;
        let err = merge(&network, &view, id("site2")).unwrap_err();
        assert!(err.contains("may not change"), "{err}");
    }

    #[test]
    fn a_restricted_client_sets_its_own_vlans_through_its_server() {
        let mut network = full();
        network
            .sites
            .iter_mut()
            .find(|s| s.name == "site2")
            .unwrap()
            .changes = Changes::Server;
        let mut view = view_for(&network, id("site2"));
        view.revision = 8;
        view.sites
            .iter_mut()
            .find(|s| s.name == "site2")
            .unwrap()
            .own_vlans = Some(VlanSet::from_range(50, 50));
        let merged = merge(&network, &view, id("site2")).unwrap().network;
        assert_eq!(
            merged.by_name("site2").unwrap().own_vlans,
            Some(VlanSet::from_range(50, 50))
        );
    }

    #[test]
    fn a_client_sees_and_changes_only_its_own_tunnels() {
        let mut network = full();
        network.set_tunnel_limit(id("alex"), id("dc"), Some(VlanSet::from_range(10, 20)));
        network.set_tunnel_limit(id("alex"), id("marcel"), Some(VlanSet::from_range(10, 30)));
        network.set_tunnel_limit(id("dc"), id("y"), Some(VlanSet::from_range(10, 40)));
        let view = view_for(&network, id("marcel"));
        assert_eq!(
            view.tunnels.len(),
            1,
            "only its tunnel to alex: {:?}",
            view.tunnels
        );
        let mut changed = view.clone();
        changed.set_tunnel_limit(id("marcel"), id("alex"), Some(VlanSet::from_range(14, 14)));
        let merged = merge(&network, &changed, id("marcel"))
            .expect("its own tunnel")
            .network;
        assert_eq!(
            merged.tunnel_limit(id("alex"), id("marcel")),
            Some(&VlanSet::from_range(14, 14))
        );
        assert_eq!(
            merged.tunnel_limit(id("alex"), id("dc")),
            Some(&VlanSet::from_range(10, 20)),
            "the tunnels it does not see stay"
        );
        assert_eq!(
            merged.tunnel_limit(id("dc"), id("y")),
            Some(&VlanSet::from_range(10, 40))
        );
        let mut foreign = view.clone();
        foreign.set_tunnel_limit(id("alex"), id("nobody"), Some(VlanSet::new()));
        let err = merge(&network, &foreign, id("marcel")).unwrap_err();
        assert!(err.contains("its own tunnels"), "{err}");
    }

    #[test]
    fn a_client_renames_itself_and_the_whole_network_follows() {
        let network = full();
        let mut view = view_for(&network, id("marcel"));
        view.rename_site(id("marcel"), "marcel2");
        view.revision += 1;
        let merged = merge(&network, &view, id("marcel")).unwrap();
        assert_eq!(merged.rekeyed, [(id("marcel"), id("marcel2"))]);
        let n = &merged.network;
        assert!(n.by_name("marcel").is_none());
        assert_eq!(n.by_name("marcel2").unwrap().uplink, Some(id("alex")));
        assert_eq!(n.by_name("x").unwrap().uplink, Some(id("marcel2")));
        assert!(n.maps.iter().any(|m| m.site == id("marcel2")));
        assert_eq!(n.revision, view.revision);
        // Merged again: nothing more to do.
        let again = merge(n, &view_for(n, id("marcel2")), id("marcel2")).unwrap();
        assert!(again.rekeyed.is_empty());
        assert_eq!(&again.network, n);
    }

    #[test]
    fn a_client_renames_its_own_client() {
        // x is a client of marcel: marcel is its server.
        let network = full();
        let mut view = view_for(&network, id("marcel"));
        view.rename_site(id("x"), "x2");
        view.revision += 1;
        let merged = merge(&network, &view, id("marcel")).unwrap();
        assert!(merged.network.by_name("x2").is_some());
    }

    #[test]
    fn a_name_of_a_site_without_identity_must_be_free_in_the_whole_network() {
        let network = full();
        let mut view = view_for(&network, id("marcel"));
        // y is a client of dc, not in marcel's view.
        view.rename_site(id("marcel"), "y");
        // Its provisional identity would be the one of y.
        assert!(merge(&network, &view, id("marcel")).is_err());
    }

    #[test]
    fn with_identities_a_client_shares_a_name() {
        let real = |n: u8| SiteId::from_bytes([n; 32]);
        let mut network = full();
        for (n, name) in [(1, "alex"), (3, "marcel"), (4, "site2"), (6, "y")] {
            network.rekey(id(name), real(n));
        }
        let mut view = view_for(&network, real(3));
        view.rename_site(real(3), "y");
        view.revision += 1;
        let merged = merge(&network, &view, real(3)).unwrap().network;
        assert_eq!(merged.named("y").count(), 2);
        assert_eq!(merged.site(real(3)).unwrap().name, "y");
    }
}
