//! Renaming sites, and sites that get another identity.
//!
//! Every reference to a site is its identity (`id`), so a rename changes
//! only its `name`. A site's identity changes in two cases: a site of an
//! older version claims its own instead of its provisional one, and a box
//! that was reset (a new identity) joins again and takes over the entry of
//! the old one. Such a site is recognised by its name and address; one
//! without an identity of its own (known by its name) also by its address
//! alone, when it is renamed. Who may rename whom is in [`crate::policy`].

use crate::config::{Local, Network};
use crate::identity::SiteId;

/// The renames from `old` to `new`, as (identity, old name, new name): a
/// site of both with another name in `new`.
pub fn renames(old: &Network, new: &Network) -> Vec<(SiteId, String, String)> {
    new.sites
        .iter()
        .filter_map(|s| {
            let before = old.site(s.id)?;
            (before.name != s.name).then(|| (s.id, before.name.clone(), s.name.clone()))
        })
        .collect()
}

/// The sites of `old` that have another identity in `new`, as (old
/// identity, new identity): a site `new` no longer has, and one `old` did
/// not have at the same address, with the same name (a claim, a box that
/// joined again), or both without an identity of their own (a site of an
/// older version renamed).
pub fn rekeys(old: &Network, new: &Network) -> Vec<(SiteId, SiteId)> {
    let mut gone: Vec<&crate::config::Site> = old
        .sites
        .iter()
        .filter(|s| new.site(s.id).is_none())
        .collect();
    let mut out = Vec::new();
    for site in new.sites.iter().filter(|s| old.site(s.id).is_none()) {
        let found = gone.iter().position(|o| {
            o.address == site.address
                && (o.name == site.name || (!o.has_identity() && !site.has_identity()))
        });
        if let Some(i) = found {
            out.push((gone.remove(i).id, site.id));
        }
    }
    out
}

/// `old` with the identities of `new` ([`rekeys`]): the rest compares like
/// two networks of the same sites. Returns it with the rekeys.
pub fn rekeyed(old: &Network, new: &Network) -> (Network, Vec<(SiteId, SiteId)>) {
    let rekeys = rekeys(old, new);
    let mut base = old.clone();
    for (from, to) in &rekeys {
        base.rekey(*from, *to);
        // A site of an older version renamed: its name comes with it.
        if let (Some(now), Some(entry)) = (new.site(*to), base.site_mut(*to))
            && !now.has_identity()
        {
            entry.name.clone_from(&now.name);
        }
    }
    (base, rekeys)
}

/// Follows in `local` the sites that have another identity from `previous`
/// to `network` (this site itself, the trunks of tunnels); returns them.
pub fn follow_rekeys(
    previous: &Network,
    network: &Network,
    local: &mut Local,
) -> Vec<(SiteId, SiteId)> {
    let rekeys = rekeys(previous, network);
    for (old, new) in &rekeys {
        local.rekey(*old, *new);
    }
    rekeys
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;
    use crate::config::{LocalVid, Site, TunnelVlans, VlanMap, VlanSet, site_pair};

    fn id(name: &str) -> SiteId {
        SiteId::provisional(name)
    }

    fn real(n: u8) -> SiteId {
        SiteId::from_bytes([n; 32])
    }

    fn site(name: &str, last: u8, uplink: Option<&str>) -> Site {
        Site {
            managed_by: uplink.map(id),
            uplink: uplink.map(id),
            ..Site::new(name, Ipv4Addr::new(10, 0, 0, last))
        }
    }

    /// hq a server, c a client of hq, x a client of c.
    fn network() -> Network {
        let mut network = Network::new(VlanSet::all());
        network.sites = vec![
            site("hq", 1, None),
            site("c", 2, Some("hq")),
            site("x", 3, Some("c")),
        ];
        network.maps = vec![VlanMap {
            site: id("c"),
            local: LocalVid::Vid(130),
            shared: 30,
        }];
        network.tunnels = vec![TunnelVlans {
            sites: site_pair(id("c"), id("hq")),
            vlans: VlanSet::from_range(30, 40),
        }];
        network
    }

    #[test]
    fn a_rename_keeps_the_identity_and_every_reference() {
        let mut old = network();
        for (n, name) in [(1, "hq"), (2, "c"), (3, "x")] {
            old.rekey(id(name), real(n));
        }
        let mut new = old.clone();
        new.rename_site(real(2), "c2");
        assert_eq!(
            renames(&old, &new),
            [(real(2), "c".to_string(), "c2".to_string())]
        );
        assert!(rekeys(&old, &new).is_empty());
        assert_eq!(new.sites[2].uplink, Some(real(2)), "references stay");
        assert!(renames(&old, &old).is_empty());
    }

    #[test]
    fn a_site_without_an_identity_is_known_by_its_name() {
        let old = network();
        // Renamed: its provisional identity follows, recognised by its
        // address.
        let mut new = old.clone();
        new.rename_site(id("c"), "c2");
        assert_eq!(new.site(id("x")).unwrap().uplink, Some(id("c2")));
        assert_eq!(new.maps[0].site, id("c2"));
        assert_eq!(new.tunnels[0].sites, site_pair(id("c2"), id("hq")));
        assert_eq!(rekeys(&old, &new), [(id("c"), id("c2"))]);
        let (base, _) = rekeyed(&old, &new);
        assert_eq!(base, new);
        // A new name with a new address: removed and added.
        let mut moved = old.clone();
        moved.rename_site(id("x"), "y");
        moved.site_mut(id("y")).unwrap().address = Ipv4Addr::new(10, 0, 0, 9);
        assert!(rekeys(&old, &moved).is_empty());
        // It claims its identity: the same name and address.
        let mut claimed = old.clone();
        claimed.rekey(id("c"), real(7));
        assert_eq!(rekeys(&old, &claimed), [(id("c"), real(7))]);
        let mut local = Local {
            self_id: Some(id("c")),
            tunnel_trunks: [(id("hq"), vec!["ens20".to_string()])].into(),
            ..Local::default()
        };
        let mut hq = old.clone();
        hq.rekey(id("hq"), real(1));
        hq.rekey(id("c"), real(7));
        assert_eq!(follow_rekeys(&old, &hq, &mut local).len(), 2);
        assert_eq!(local.self_id, Some(real(7)));
        assert!(local.tunnel_trunks.contains_key(&real(1)));
    }

    #[test]
    fn a_box_that_joined_again_takes_over_the_entry() {
        let mut old = network();
        old.rekey(id("x"), real(3));
        let mut new = old.clone();
        new.rekey(real(3), real(9));
        assert_eq!(rekeys(&old, &new), [(real(3), real(9))]);
        // Another box at that address with another name is another site.
        let mut other = old.clone();
        other.sites[2].id = real(9);
        other.sites[2].name = "z".into();
        assert!(rekeys(&old, &other).is_empty());
    }
}
