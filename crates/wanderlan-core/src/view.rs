//! What each site knows of the network.
//!
//! Servers (sites without uplink) know the whole network. A client knows
//! only itself, the site it is plugged into (its uplink, which it sees as a
//! server) and its own clients; never the clients of other servers. Its
//! `mesh.toml` contains exactly that.
//!
//! A client's changes reach the network through its uplink: the uplink
//! fetches the client's view and merges what the client is responsible for
//! (itself, its own clients and the network settings) into its own mesh.

use std::collections::BTreeSet;

use crate::config::{Mesh, Site, VlanMap, fingerprint, validate};
use crate::policy;

/// The part of `mesh` the site `site` knows: everything for a server; for a
/// client itself, its uplink (without the uplink's own uplink) and its own
/// clients. An unknown site knows no sites at all.
pub fn view_for(mesh: &Mesh, site: &str) -> Mesh {
    let Some(me) = mesh.site(site) else {
        return Mesh {
            sites: Vec::new(),
            maps: Vec::new(),
            ..mesh.clone()
        };
    };
    let Some(uplink) = me.uplink.as_deref() else {
        return mesh.clone();
    };
    let sites: Vec<Site> = mesh
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
    let maps = mesh
        .maps
        .iter()
        .filter(|m| sites.iter().any(|s| s.name == m.site))
        .cloned()
        .collect();
    Mesh {
        sites,
        maps,
        ..mesh.clone()
    }
}

fn maps_of<'a>(mesh: &'a Mesh, site: &str) -> Vec<&'a VlanMap> {
    let mut maps: Vec<&VlanMap> = mesh.maps.iter().filter(|m| m.site == site).collect();
    maps.sort_by_key(|m| (m.local, m.shared));
    maps
}

/// Merges the view of the client `from` into `full` (the mesh of its
/// uplink): its own entry, the entries of its own clients (new ones too,
/// removed ones go), their mappings and the network settings. Anything else
/// in the view must be as `from` received it. The result is checked against
/// the rights of `from` and validated.
pub fn merge(full: &Mesh, view: &Mesh, from: &str) -> Result<Mesh, String> {
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
            online: true,
            managed_by: uplink.map(Into::into),
            self_managed: true,
            uplink: uplink.map(Into::into),
        }
    }

    /// alex and dc are servers; marcel and site2 are clients of alex, x is a
    /// client of marcel, y a client of dc.
    fn full() -> Mesh {
        let mut mesh = Mesh::new(VlanSet::from_range(10, 50));
        mesh.revision = 7;
        mesh.sites = vec![
            site("alex", 1, None),
            site("dc", 2, None),
            site("marcel", 3, Some("alex")),
            site("site2", 4, Some("alex")),
            site("x", 5, Some("marcel")),
            site("y", 6, Some("dc")),
        ];
        mesh.maps = vec![
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
        mesh
    }

    fn names(mesh: &Mesh) -> Vec<&str> {
        mesh.sites.iter().map(|s| s.name.as_str()).collect()
    }

    #[test]
    fn a_client_knows_its_server_and_its_own_clients_only() {
        let mesh = full();
        assert_eq!(
            names(&view_for(&mesh, "alex")),
            names(&mesh),
            "a server knows all"
        );
        let site2 = view_for(&mesh, "site2");
        assert_eq!(names(&site2), ["alex", "site2"]);
        assert_eq!(site2.maps.len(), 1, "only its own mapping");
        assert_eq!(site2.revision, 7);
        let marcel = view_for(&mesh, "marcel");
        assert_eq!(names(&marcel), ["alex", "marcel", "x"]);
        let x = view_for(&mesh, "x");
        assert_eq!(names(&x), ["marcel", "x"]);
        assert_eq!(
            x.site("marcel").unwrap().uplink,
            None,
            "its uplink is its server"
        );
        assert!(view_for(&mesh, "nobody").sites.is_empty());
        // Every view is a valid configuration of its own.
        for s in ["site2", "marcel", "x", "y"] {
            let v = view_for(&mesh, s);
            assert!(!validate(&v, None).iter().any(|d| d.is_error()), "{s}");
        }
    }

    #[test]
    fn the_uplink_merges_what_the_client_is_responsible_for() {
        let mesh = full();
        let mut view = view_for(&mesh, "marcel");
        view.revision = 8;
        // marcel changes its client x and invites z.
        view.sites
            .iter_mut()
            .find(|s| s.name == "x")
            .unwrap()
            .online = false;
        view.sites.push(site("z", 9, Some("marcel")));
        let merged = merge(&mesh, &view, "marcel").unwrap();
        assert_eq!(merged.revision, 8);
        assert!(!merged.site("x").unwrap().online);
        assert!(merged.site("z").is_some());
        assert!(
            merged.site("y").is_some() && merged.site("site2").is_some(),
            "the rest stays"
        );
        assert_eq!(merged.site("alex").unwrap(), mesh.site("alex").unwrap());

        // Removing its own client.
        let mut view = view_for(&mesh, "marcel");
        view.revision = 8;
        view.sites.retain(|s| s.name != "x");
        let merged = merge(&mesh, &view, "marcel").unwrap();
        assert!(merged.site("x").is_none());

        // An older view with changes still gets a newer revision.
        let mut view = view_for(&mesh, "site2");
        view.sites
            .iter_mut()
            .find(|s| s.name == "site2")
            .unwrap()
            .vlans = Some(VlanSet::from_range(13, 13));
        let merged = merge(&mesh, &view, "site2").unwrap();
        assert_eq!(merged.revision, 8);
        // Nothing changed: the revision is not raised.
        let same = merge(&mesh, &view_for(&mesh, "site2"), "site2").unwrap();
        assert_eq!(same, mesh);
    }

    #[test]
    fn a_client_cannot_change_its_server_or_take_other_names() {
        let mesh = full();
        let mut view = view_for(&mesh, "site2");
        view.revision = 8;
        view.sites
            .iter_mut()
            .find(|s| s.name == "alex")
            .unwrap()
            .online = false;
        let err = merge(&mesh, &view, "site2").unwrap_err();
        assert!(
            err.contains("may only change itself and its own clients"),
            "{err}"
        );

        let mut view = view_for(&mesh, "site2");
        view.sites.push(site("marcel", 20, Some("site2")));
        let err = merge(&mesh, &view, "site2").unwrap_err();
        assert!(err.contains("already used"), "{err}");

        let mut view = view_for(&mesh, "site2");
        view.sites.retain(|s| s.name != "alex");
        assert!(merge(&mesh, &view, "site2").is_err());
        assert!(
            merge(&mesh, &view_for(&mesh, "alex"), "site2").is_err(),
            "wrong sender"
        );
    }

    #[test]
    fn the_rights_of_the_client_apply() {
        let mut mesh = full();
        // site2 is managed by alex and may not change itself.
        mesh.sites
            .iter_mut()
            .find(|s| s.name == "site2")
            .unwrap()
            .self_managed = false;
        let mut view = view_for(&mesh, "site2");
        view.revision = 8;
        view.sites
            .iter_mut()
            .find(|s| s.name == "site2")
            .unwrap()
            .online = false;
        let err = merge(&mesh, &view, "site2").unwrap_err();
        assert!(err.contains("may not change"), "{err}");
    }
}
