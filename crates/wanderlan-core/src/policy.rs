//! Who may change what.
//!
//! A site that joined with an invitation is managed by the site that created
//! the invitation (`managed_by`): only that site may change the rules of the
//! joined site, i.e. its VLANs, its VLAN mappings, whether it is online, who
//! manages it, and whether it stays in the network at all.
//!
//! A managed site that was not allowed to manage itself is **restricted**:
//! it may not change any VLAN rules, neither its own, nor those of other
//! sites, nor the VLANs of the network; only those of sites it invited
//! itself. With `self_managed` ("may change its own VLANs") it has the same
//! rights as the site that invited it.
//!
//! Sites without a manager (the first site, sites added by hand) and the
//! network's VLANs may be changed by every site that is not restricted. A
//! manager that left the network can no longer decide: its sites are open
//! again and no longer restricted.
//!
//! Every wanderland checks this when a change is made on it (TUI, CLI,
//! edited files). It is an agreement between the daemons, not a
//! cryptographic protection.

use crate::config::{Network, Site, VlanMap};

/// The parts of a site that only its managers may change.
#[derive(Debug, PartialEq, Eq)]
struct Rules<'a> {
    address: std::net::Ipv4Addr,
    vlans: Option<String>,
    online: bool,
    managed_by: Option<&'a str>,
    self_managed: bool,
    uplink: Option<&'a str>,
    maps: Vec<&'a VlanMap>,
}

fn rules<'a>(network: &'a Network, site: &'a Site) -> Rules<'a> {
    let mut maps: Vec<&VlanMap> = network.maps_of(&site.name).collect();
    maps.sort_by_key(|m| (m.local, m.shared));
    Rules {
        address: site.address,
        vlans: site.vlans.as_ref().map(ToString::to_string),
        online: site.online,
        managed_by: site.managed_by.as_deref(),
        self_managed: site.self_managed,
        uplink: site.uplink.as_deref(),
        maps,
    }
}

/// The manager of `site` if it is still part of the network.
pub fn manager<'a>(network: &Network, site: &'a Site) -> Option<&'a str> {
    site.managed_by
        .as_deref()
        .filter(|m| network.site(m).is_some())
}

/// The site that manages `editor` if `editor` is restricted (joined with an
/// invitation and not allowed to manage itself). A site that is managed by
/// itself ("only alex may change alex") is never restricted.
pub fn restricted_by<'a>(network: &'a Network, editor: &str) -> Option<&'a str> {
    let site = network.site(editor)?;
    if site.self_managed {
        return None;
    }
    manager(network, site).filter(|m| *m != editor)
}

/// True if `editor` may change the rules of `site` in `network`.
pub fn may_change(network: &Network, site: &Site, editor: &str) -> bool {
    // A client only changes itself and its own clients, never its server.
    if client_of(network, editor).is_some()
        && site.name != editor
        && site.uplink.as_deref() != Some(editor)
    {
        return false;
    }
    match manager(network, site) {
        Some(m) => editor == m || (editor == site.name && site.self_managed),
        None => restricted_by(network, editor).is_none(),
    }
}

/// The site `editor` is a client of, if it is a client.
pub fn client_of<'a>(network: &'a Network, editor: &str) -> Option<&'a str> {
    network.site(editor)?.uplink.as_deref()
}

/// True if `editor` may change the VLANs of the network (`network.vlans`).
pub fn may_change_network_vlans(network: &Network, editor: &str) -> bool {
    restricted_by(network, editor).is_none()
}

/// Explains who may change `site`, for displays.
pub fn managers(network: &Network, site: &Site) -> String {
    match (manager(network, site), site.self_managed) {
        (None, _) => "every site that is not managed by another".into(),
        (Some(m), false) => format!("only \"{m}\""),
        (Some(m), true) => format!("\"{m}\" and \"{}\" itself", site.name),
    }
}

/// Short form for tables: `anyone`, `a`, `a+self`.
pub fn managed_label(network: &Network, site: &Site) -> String {
    match (manager(network, site), site.self_managed) {
        (None, _) => "anyone".into(),
        (Some(m), false) => m.to_string(),
        (Some(m), true) => format!("{m}+self"),
    }
}

/// Why `editor` may not `what` (change, remove) `site`; only meaningful if
/// it may not.
pub fn why_not(network: &Network, site: &Site, editor: &str, what: &str) -> String {
    let name = &site.name;
    if let Some(up) = client_of(network, editor)
        && site.name != editor
        && site.uplink.as_deref() != Some(editor)
    {
        return format!(
            "this site (\"{editor}\") is a client of \"{up}\": it may only change itself and its own clients, not {what} site \"{name}\""
        );
    }
    if let Some(m) = restricted_by(network, editor)
        && manager(network, site) != Some(editor)
    {
        return format!(
            "this site (\"{editor}\") may not {what} site \"{name}\": {}",
            restricted_reason(m)
        );
    }
    format!(
        "this site (\"{editor}\") may not {what} site \"{name}\": its VLANs, mappings and online state are managed by {}; make the change there",
        managers(network, site)
    )
}

/// Why `editor` may not change the network's VLANs; empty if it may.
pub fn why_not_network(network: &Network, editor: &str) -> String {
    restricted_by(network, editor).map_or_else(String::new, |m| {
        format!(
            "this site (\"{editor}\") may not change the VLANs of the network: {}",
            restricted_reason(m)
        )
    })
}

fn restricted_reason(manager: &str) -> String {
    format!(
        "it is managed by \"{manager}\" and may not change VLANs, mappings or the online state (except of sites it invited itself); make the change on \"{manager}\""
    )
}

/// Checks that `editor` only changes what it may: the network's VLANs, and
/// every site of `old` whose rules differ in `new` (or that is gone).
pub fn check_edit(old: &Network, new: &Network, editor: &str) -> Result<(), String> {
    if old.vlans != new.vlans && !may_change_network_vlans(old, editor) {
        return Err(why_not_network(old, editor));
    }
    for site in &old.sites {
        if may_change(old, site, editor) {
            continue;
        }
        let what = match new.site(&site.name) {
            None => "remove",
            Some(now) if rules(new, now) != rules(old, site) => "change",
            Some(_) => continue,
        };
        return Err(why_not(old, site, editor, what));
    }
    // Its own VLANs: the site itself, or whoever may change it.
    for site in &old.sites {
        if let Some(now) = new.site(&site.name)
            && now.own_vlans != site.own_vlans
            && editor != site.name
            && !may_change(old, site, editor)
        {
            return Err(why_not(old, site, editor, "change the own VLANs of"));
        }
    }
    // A client only adds clients of its own.
    if let Some(up) = client_of(old, editor)
        && let Some(added) = new
            .sites
            .iter()
            .find(|s| old.site(&s.name).is_none() && s.uplink.as_deref() != Some(editor))
    {
        return Err(format!(
            "this site (\"{editor}\") is a client of \"{up}\": it may only add clients of its own, not \"{}\"",
            added.name
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;
    use crate::config::{LocalVid, VlanSet};

    fn site(name: &str, last: u8, managed_by: Option<&str>, self_managed: bool) -> Site {
        Site {
            name: name.into(),
            address: Ipv4Addr::new(10, 0, 0, last),
            vlans: None,
            own_vlans: None,
            online: true,
            managed_by: managed_by.map(Into::into),
            self_managed,
            uplink: None,
        }
    }

    fn network() -> Network {
        let mut network = Network::new(VlanSet::all());
        network.sites = vec![
            site("a", 1, None, false),
            site("b", 2, None, false),
            site("c", 3, Some("a"), false),
            site("d", 4, Some("a"), true),
        ];
        network
    }

    #[test]
    fn only_the_inviting_site_changes_an_invited_site() {
        let old = network();
        let mut new = old.clone();
        new.sites[2].vlans = Some(VlanSet::from_range(10, 20));
        assert!(check_edit(&old, &new, "a").is_ok());
        let err = check_edit(&old, &new, "b").unwrap_err();
        assert!(err.contains("may not change site \"c\""), "{err}");
        assert!(err.contains("only \"a\""), "{err}");
        assert!(check_edit(&old, &new, "c").is_err(), "not self-managed");
    }

    #[test]
    fn a_self_managed_site_changes_itself() {
        let old = network();
        let mut new = old.clone();
        new.sites[3].online = false;
        assert!(check_edit(&old, &new, "d").is_ok());
        assert!(check_edit(&old, &new, "a").is_ok());
        assert!(check_edit(&old, &new, "b").is_err());
    }

    #[test]
    fn mappings_removal_and_management_are_protected() {
        let old = network();
        let mut new = old.clone();
        new.maps.push(VlanMap {
            site: "c".into(),
            local: LocalVid::Native,
            shared: 20,
        });
        assert!(check_edit(&old, &new, "b").is_err());
        let mut gone = old.clone();
        gone.sites.retain(|s| s.name != "c");
        assert!(
            check_edit(&old, &gone, "b")
                .unwrap_err()
                .contains("may not remove")
        );
        let mut taken = old.clone();
        taken.sites[2].managed_by = Some("b".into());
        assert!(check_edit(&old, &taken, "b").is_err());
        // Which site it is plugged into is a rule too.
        let mut moved = old.clone();
        moved.sites[2].uplink = Some("b".into());
        assert!(check_edit(&old, &moved, "b").is_err());
        assert!(check_edit(&old, &moved, "a").is_ok());
    }

    #[test]
    fn open_sites_and_network_settings_are_open_to_unrestricted_sites() {
        let old = network();
        let mut new = old.clone();
        new.sites[1].vlans = Some(VlanSet::from_range(30, 30));
        new.commit_timeout = std::time::Duration::from_secs(120);
        new.vlans = VlanSet::from_range(10, 50);
        assert!(check_edit(&old, &new, "a").is_ok());
        assert!(check_edit(&old, &new, "b").is_ok());
        assert!(
            check_edit(&old, &new, "d").is_ok(),
            "released: like its manager"
        );
        // Without its manager, an invited site is open again.
        let mut orphan = old.clone();
        orphan.sites.retain(|s| s.name != "a");
        let mut changed = orphan.clone();
        changed.sites[1].online = false;
        assert!(check_edit(&orphan, &changed, "b").is_ok());
        assert!(
            check_edit(&orphan, &changed, "c").is_ok(),
            "no longer restricted"
        );
    }

    #[test]
    fn a_restricted_site_changes_no_vlan_rules() {
        let old = network();
        assert_eq!(restricted_by(&old, "c"), Some("a"));
        assert_eq!(restricted_by(&old, "d"), None, "released");
        assert_eq!(restricted_by(&old, "a"), None);
        // The network's VLANs.
        let mut new = old.clone();
        new.vlans = VlanSet::from_range(10, 50);
        let err = check_edit(&old, &new, "c").unwrap_err();
        assert!(err.contains("managed by \"a\""), "{err}");
        // An open site (the inviting one).
        let mut new = old.clone();
        new.sites[0].vlans = Some(VlanSet::from_range(10, 20));
        assert!(check_edit(&old, &new, "c").is_err());
        // A mapping of an open site.
        let mut new = old.clone();
        new.maps.push(VlanMap {
            site: "b".into(),
            local: LocalVid::Native,
            shared: 20,
        });
        assert!(check_edit(&old, &new, "c").is_err());
        // Technical settings stay open.
        let mut new = old.clone();
        new.commit_timeout = std::time::Duration::from_secs(300);
        assert!(check_edit(&old, &new, "c").is_ok());
    }

    #[test]
    fn a_restricted_site_manages_the_sites_it_invited() {
        let mut old = network();
        old.sites.push(site("e", 5, Some("c"), false));
        let mut new = old.clone();
        new.sites[4].online = false;
        assert!(check_edit(&old, &new, "c").is_ok());
        assert!(
            check_edit(&old, &new, "a").is_err(),
            "e is managed by c only"
        );
    }

    #[test]
    fn labels_and_messages() {
        let m = network();
        assert_eq!(managed_label(&m, &m.sites[0]), "anyone");
        assert_eq!(managed_label(&m, &m.sites[2]), "a");
        assert_eq!(managed_label(&m, &m.sites[3]), "a+self");
        assert!(why_not(&m, &m.sites[0], "c", "change").contains("make the change on \"a\""));
        assert!(why_not(&m, &m.sites[2], "b", "change").contains("only \"a\""));
        assert!(may_change_network_vlans(&m, "b"));
        assert!(!may_change_network_vlans(&m, "c"));
    }

    #[test]
    fn a_client_changes_only_itself_and_its_own_clients() {
        let mut old = network();
        // d is a client of a (released, so its VLANs are its own), e a client of d.
        old.sites[3].uplink = Some("a".into());
        old.sites.push(Site {
            uplink: Some("d".into()),
            managed_by: Some("d".into()),
            ..site("e", 5, None, false)
        });
        let mut new = old.clone();
        new.sites[3].online = false;
        new.sites[4].online = false;
        assert!(check_edit(&old, &new, "d").is_ok(), "itself and its client");
        let mut server = old.clone();
        server.sites[1].online = false;
        let err = check_edit(&old, &server, "d").unwrap_err();
        assert!(err.contains("is a client of \"a\""), "{err}");
        let mut added = old.clone();
        added.sites.push(site("f", 6, None, false));
        assert!(
            check_edit(&old, &added, "d")
                .unwrap_err()
                .contains("only add clients")
        );
        added.sites[5].uplink = Some("d".into());
        assert!(check_edit(&old, &added, "d").is_ok());
    }

    #[test]
    fn a_restricted_site_sets_its_own_vlans() {
        let old = network();
        assert_eq!(restricted_by(&old, "c"), Some("a"));
        let mut new = old.clone();
        new.sites[2].own_vlans = Some(VlanSet::from_range(60, 60));
        assert!(check_edit(&old, &new, "c").is_ok(), "c itself");
        assert!(check_edit(&old, &new, "a").is_ok(), "its manager");
        let err = check_edit(&old, &new, "b").unwrap_err();
        assert!(
            err.contains("may not change the own VLANs of site \"c\""),
            "{err}"
        );
    }

    #[test]
    fn a_site_managed_by_itself_is_not_restricted() {
        let mut old = network();
        old.sites[0].managed_by = Some("a".into());
        assert_eq!(restricted_by(&old, "a"), None);
        assert!(may_change_network_vlans(&old, "a"));
        let mut new = old.clone();
        new.vlans = VlanSet::from_range(10, 20);
        new.sites[0].vlans = Some(VlanSet::from_range(10, 10));
        new.sites[2].online = false;
        assert!(
            check_edit(&old, &new, "a").is_ok(),
            "network, itself, its clients"
        );
        // Others still may not change it.
        let mut other = old.clone();
        other.sites[0].online = false;
        assert!(check_edit(&old, &other, "b").is_err());
    }
}
