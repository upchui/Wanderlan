//! What each site knows of the network.
//!
//! Servers (sites without uplink) know the whole network. A client knows
//! only itself, the site it is plugged into (its uplink, which it sees as a
//! server) and its own clients; never the clients of other servers. Its
//! `network.toml` contains exactly that.
//!
//! A client's changes reach the network through its uplink: the uplink
//! fetches the client's view and merges what the client is responsible for
//! (itself, its own clients and the network settings) into its own network.

use std::collections::BTreeSet;

use crate::config::{Network, Site, TunnelVlans, VlanMap, fingerprint, validate};
use crate::policy;

/// The part of `network` the site `site` knows: everything for a server; for a
/// client itself, its uplink (without the uplink's own uplink) and its own
/// clients. An unknown site knows no sites at all.
pub fn view_for(network: &Network, site: &str) -> Network {
    let Some(me) = network.site(site) else {
        return Network {
            sites: Vec::new(),
            maps: Vec::new(),
            tunnels: Vec::new(),
            ..network.clone()
        };
    };
    let Some(uplink) = me.uplink.as_deref() else {
        return network.clone();
    };
    let sites: Vec<Site> = network
        .sites
        .iter()
        .filter(|s| s.name == site || s.name == uplink || s.uplink.as_deref() == Some(site))
        .map(|s| {
            let mut s = s.clone();
            if s.name == uplink {
                // Seen from here, the uplink is a server.
                s.uplink = None;
            }
            s
        })
        .collect();
    let maps = network
        .maps
        .iter()
        .filter(|m| sites.iter().any(|s| s.name == m.site))
        .cloned()
        .collect();
    let tunnels = network
        .tunnels
        .iter()
        .filter(|t| {
            t.sites
                .iter()
                .all(|name| sites.iter().any(|s| &s.name == name))
        })
        .cloned()
        .collect();
    Network {
        sites,
        maps,
        tunnels,
        ..network.clone()
    }
}

fn maps_of<'a>(network: &'a Network, site: &str) -> Vec<&'a VlanMap> {
    let mut maps: Vec<&VlanMap> = network.maps.iter().filter(|m| m.site == site).collect();
    maps.sort_by_key(|m| (m.local, m.shared));
    maps
}

/// What [`merge`] took over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merged {
    /// The network with the client's changes.
    pub network: Network,
    /// Sites the client renamed (itself, its own clients), as (old, new).
    pub renamed: Vec<(String, String)>,
}

/// Merges the view of the client `from` into `full` (the network of its
/// uplink): its own entry, the entries of its own clients (new ones too,
/// removed ones go), their mappings and the network settings. Anything else
/// in the view must be as `from` received it. A site of the view at the
/// address of a site under another name is that site renamed (the client
/// itself, too); the rename reaches the whole network. The result is checked
/// against the rights of `from` and validated.
pub fn merge(full: &Network, view: &Network, from: &str) -> Result<Merged, String> {
    let renamed = crate::rename::renames(&view_for(full, from), view);
    let mut base = full.clone();
    for (old, new) in &renamed {
        if full.site(new).is_some() {
            return Err(format!("the name \"{new}\" is already used in the network"));
        }
        base.rename_site(old, new);
    }
    let from_now = renamed
        .iter()
        .find(|(old, _)| old == from)
        .map_or(from, |(_, new)| new.as_str());
    let mut merged = merge_named(&base, view, from_now)?;
    merged.revision = if fingerprint(&merged) == fingerprint(full) {
        full.revision.max(view.revision)
    } else if view.revision > full.revision {
        view.revision
    } else {
        full.revision + 1
    };
    policy::check_edit(full, &merged, from)?;
    if let Some(error) = validate(&merged, None).into_iter().find(|d| d.is_error()) {
        return Err(error.message);
    }
    Ok(Merged {
        network: merged,
        renamed,
    })
}

/// [`merge`] after the renames: `full` already has the new names.
fn merge_named(full: &Network, view: &Network, from: &str) -> Result<Network, String> {
    if view.site(from).is_none() {
        return Err(format!(
            "the configuration of \"{from}\" does not contain \"{from}\" itself"
        ));
    }
    let owned = |s: &Site| s.name == from || s.uplink.as_deref() == Some(from);
    let expected = view_for(full, from);
    for site in &view.sites {
        if owned(site) {
            if full.site(&site.name).is_some_and(|f| !owned(f)) {
                return Err(format!(
                    "the name \"{}\" is already used in the network",
                    site.name
                ));
            }
            continue;
        }
        let same = expected.site(&site.name) == Some(site)
            && maps_of(view, &site.name) == maps_of(&expected, &site.name);
        if !same {
            return Err(format!(
                "\"{from}\" may only change itself and its own clients, not \"{}\"",
                site.name
            ));
        }
    }
    if let Some(missing) = expected
        .sites
        .iter()
        .find(|s| !owned(s) && view.site(&s.name).is_none())
    {
        return Err(format!(
            "\"{from}\" may only change itself and its own clients, not remove \"{}\"",
            missing.name
        ));
    }

    let mut merged = full.clone();
    merged.vlans = view.vlans.clone();
    merged.control_port = view.control_port;
    merged.mtu_probe_interval = view.mtu_probe_interval;
    merged.commit_timeout = view.commit_timeout;
    merged.mss = view.mss;
    // Its own sites as in the view (missing ones are removed), in place.
    merged.sites = full
        .sites
        .iter()
        .filter_map(|s| {
            if owned(s) {
                view.site(&s.name).filter(|v| owned(v)).cloned()
            } else {
                Some(s.clone())
            }
        })
        .collect();
    for site in view
        .sites
        .iter()
        .filter(|s| owned(s) && full.site(&s.name).is_none())
    {
        merged.sites.push(site.clone());
    }
    let names: BTreeSet<&str> = full
        .sites
        .iter()
        .chain(view.sites.iter())
        .filter(|s| owned(s))
        .map(|s| s.name.as_str())
        .collect();
    // Its own mappings as in the view; unchanged ones keep their place.
    let own_maps: Vec<&VlanMap> = view
        .maps
        .iter()
        .filter(|m| names.contains(m.site.as_str()) && view.site(&m.site).is_some_and(owned))
        .collect();
    merged
        .maps
        .retain(|m| !names.contains(m.site.as_str()) || own_maps.contains(&m));
    for map in own_maps {
        if !merged.maps.contains(map) {
            merged.maps.push(map.clone());
        }
    }

    // The VLANs of its own tunnels as in the view; the others as received.
    let touches_own = |t: &TunnelVlans| t.sites.iter().any(|s| names.contains(s.as_str()));
    let foreign = |n: &Network| -> Vec<TunnelVlans> {
        let mut list: Vec<TunnelVlans> = n
            .tunnels
            .iter()
            .filter(|t| !touches_own(t))
            .cloned()
            .collect();
        list.sort_by(|x, y| x.sites.cmp(&y.sites));
        list
    };
    if foreign(view) != foreign(&expected) {
        return Err(format!(
            "\"{from}\" may only change the VLANs of its own tunnels"
        ));
    }
    merged
        .tunnels
        .retain(|t| !touches_own(t) || view.tunnels.contains(t));
    for tunnel in view.tunnels.iter().filter(|t| touches_own(t)) {
        if !merged.tunnels.contains(tunnel) {
            merged.tunnels.push(tunnel.clone());
        }
    }

    Ok(merged)
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;
    use crate::config::{LocalVid, VlanSet};

    fn site(name: &str, last: u8, uplink: Option<&str>) -> Site {
        Site {
            name: name.into(),
            address: Ipv4Addr::new(10, 0, 0, last),
            vlans: None,
            own_vlans: None,
            online: true,
            managed_by: uplink.map(Into::into),
            self_managed: true,
            uplink: uplink.map(Into::into),
        }
    }

    /// alex and dc are servers; marcel and site2 are clients of alex, x is a
    /// client of marcel, y a client of dc.
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
        ];
        network.maps = vec![
            VlanMap {
                site: "site2".into(),
                local: LocalVid::Native,
                shared: 13,
            },
            VlanMap {
                site: "marcel".into(),
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
    fn a_client_knows_its_server_and_its_own_clients_only() {
        let network = full();
        assert_eq!(
            names(&view_for(&network, "alex")),
            names(&network),
            "a server knows all"
        );
        let site2 = view_for(&network, "site2");
        assert_eq!(names(&site2), ["alex", "site2"]);
        assert_eq!(site2.maps.len(), 1, "only its own mapping");
        assert_eq!(site2.revision, 7);
        let marcel = view_for(&network, "marcel");
        assert_eq!(names(&marcel), ["alex", "marcel", "x"]);
        let x = view_for(&network, "x");
        assert_eq!(names(&x), ["marcel", "x"]);
        assert_eq!(
            x.site("marcel").unwrap().uplink,
            None,
            "its uplink is its server"
        );
        assert!(view_for(&network, "nobody").sites.is_empty());
        // Every view is a valid configuration of its own.
        for s in ["site2", "marcel", "x", "y"] {
            let v = view_for(&network, s);
            assert!(!validate(&v, None).iter().any(|d| d.is_error()), "{s}");
        }
    }

    #[test]
    fn the_uplink_merges_what_the_client_is_responsible_for() {
        let network = full();
        let mut view = view_for(&network, "marcel");
        view.revision = 8;
        // marcel changes its client x and invites z.
        view.sites
            .iter_mut()
            .find(|s| s.name == "x")
            .unwrap()
            .online = false;
        view.sites.push(site("z", 9, Some("marcel")));
        let merged = merge(&network, &view, "marcel").unwrap().network;
        assert_eq!(merged.revision, 8);
        assert!(!merged.site("x").unwrap().online);
        assert!(merged.site("z").is_some());
        assert!(
            merged.site("y").is_some() && merged.site("site2").is_some(),
            "the rest stays"
        );
        assert_eq!(merged.site("alex").unwrap(), network.site("alex").unwrap());

        // Removing its own client.
        let mut view = view_for(&network, "marcel");
        view.revision = 8;
        view.sites.retain(|s| s.name != "x");
        let merged = merge(&network, &view, "marcel").unwrap().network;
        assert!(merged.site("x").is_none());

        // An older view with changes still gets a newer revision.
        let mut view = view_for(&network, "site2");
        view.sites
            .iter_mut()
            .find(|s| s.name == "site2")
            .unwrap()
            .vlans = Some(VlanSet::from_range(13, 13));
        let merged = merge(&network, &view, "site2").unwrap().network;
        assert_eq!(merged.revision, 8);
        // Nothing changed: the revision is not raised.
        let same = merge(&network, &view_for(&network, "site2"), "site2")
            .unwrap()
            .network;
        assert_eq!(same, network);
    }

    #[test]
    fn a_client_cannot_change_its_server_or_take_other_names() {
        let network = full();
        let mut view = view_for(&network, "site2");
        view.revision = 8;
        view.sites
            .iter_mut()
            .find(|s| s.name == "alex")
            .unwrap()
            .online = false;
        let err = merge(&network, &view, "site2").unwrap_err();
        assert!(
            err.contains("may only change itself and its own clients"),
            "{err}"
        );

        let mut view = view_for(&network, "site2");
        view.sites.push(site("marcel", 20, Some("site2")));
        let err = merge(&network, &view, "site2").unwrap_err();
        assert!(err.contains("already used"), "{err}");

        let mut view = view_for(&network, "site2");
        view.sites.retain(|s| s.name != "alex");
        assert!(merge(&network, &view, "site2").is_err());
        assert!(
            merge(&network, &view_for(&network, "alex"), "site2").is_err(),
            "wrong sender"
        );
    }

    #[test]
    fn the_rights_of_the_client_apply() {
        let mut network = full();
        // site2 is managed by alex and may not change itself.
        network
            .sites
            .iter_mut()
            .find(|s| s.name == "site2")
            .unwrap()
            .self_managed = false;
        let mut view = view_for(&network, "site2");
        view.revision = 8;
        view.sites
            .iter_mut()
            .find(|s| s.name == "site2")
            .unwrap()
            .online = false;
        let err = merge(&network, &view, "site2").unwrap_err();
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
            .self_managed = false;
        let mut view = view_for(&network, "site2");
        view.revision = 8;
        view.sites
            .iter_mut()
            .find(|s| s.name == "site2")
            .unwrap()
            .own_vlans = Some(VlanSet::from_range(50, 50));
        let merged = merge(&network, &view, "site2").unwrap().network;
        assert_eq!(
            merged.site("site2").unwrap().own_vlans,
            Some(VlanSet::from_range(50, 50))
        );
    }

    #[test]
    fn a_client_sees_and_changes_only_its_own_tunnels() {
        let mut network = full();
        network.set_tunnel_limit("alex", "dc", Some(VlanSet::from_range(10, 20)));
        network.set_tunnel_limit("alex", "marcel", Some(VlanSet::from_range(10, 30)));
        network.set_tunnel_limit("dc", "y", Some(VlanSet::from_range(10, 40)));
        let view = view_for(&network, "marcel");
        assert_eq!(
            view.tunnels.len(),
            1,
            "only its tunnel to alex: {:?}",
            view.tunnels
        );
        let mut changed = view.clone();
        changed.set_tunnel_limit("marcel", "alex", Some(VlanSet::from_range(14, 14)));
        let merged = merge(&network, &changed, "marcel")
            .expect("its own tunnel")
            .network;
        assert_eq!(
            merged.tunnel_limit("alex", "marcel"),
            Some(&VlanSet::from_range(14, 14))
        );
        assert_eq!(
            merged.tunnel_limit("alex", "dc"),
            Some(&VlanSet::from_range(10, 20)),
            "the tunnels it does not see stay"
        );
        assert_eq!(
            merged.tunnel_limit("dc", "y"),
            Some(&VlanSet::from_range(10, 40))
        );
        let mut foreign = view.clone();
        foreign.set_tunnel_limit("alex", "nobody", Some(VlanSet::new()));
        let err = merge(&network, &foreign, "marcel").unwrap_err();
        assert!(err.contains("its own tunnels"), "{err}");
    }

    #[test]
    fn a_client_renames_itself_and_the_whole_network_follows() {
        let mut network = full();
        // y (a client of dc) was invited by marcel: outside marcel's view.
        network.sites[5].managed_by = Some("marcel".into());
        let mut view = view_for(&network, "marcel");
        view.rename_site("marcel", "marcel2");
        view.revision += 1;
        let merged = merge(&network, &view, "marcel").unwrap();
        assert_eq!(
            merged.renamed,
            [("marcel".to_string(), "marcel2".to_string())]
        );
        let n = &merged.network;
        assert!(n.site("marcel").is_none());
        assert_eq!(n.site("marcel2").unwrap().uplink.as_deref(), Some("alex"));
        assert_eq!(n.site("x").unwrap().uplink.as_deref(), Some("marcel2"));
        assert_eq!(n.site("y").unwrap().managed_by.as_deref(), Some("marcel2"));
        assert!(n.maps.iter().any(|m| m.site == "marcel2"));
        assert_eq!(n.revision, view.revision);
        // Merged again: nothing more to do.
        let again = merge(n, &view_for(n, "marcel2"), "marcel2").unwrap();
        assert!(again.renamed.is_empty());
        assert_eq!(&again.network, n);
    }

    #[test]
    fn a_client_renames_its_own_client_if_it_manages_it() {
        // x is a client of marcel and managed by it.
        let network = full();
        let mut view = view_for(&network, "marcel");
        view.rename_site("x", "x2");
        view.revision += 1;
        let merged = merge(&network, &view, "marcel").unwrap();
        assert!(merged.network.site("x2").is_some());
        // Managed by another site: not by marcel.
        let mut other = network.clone();
        other.sites[4].managed_by = Some("alex".into());
        let mut view = view_for(&other, "marcel");
        view.rename_site("x", "x2");
        view.revision += 1;
        let err = merge(&other, &view, "marcel").unwrap_err();
        assert!(err.contains("may not rename site \"x\""), "{err}");
    }

    #[test]
    fn a_new_name_must_be_free_in_the_whole_network() {
        let network = full();
        let mut view = view_for(&network, "marcel");
        // y is a client of dc, not in marcel's view.
        view.rename_site("marcel", "y");
        let err = merge(&network, &view, "marcel").unwrap_err();
        assert!(err.contains("\"y\" is already used"), "{err}");
    }
}
