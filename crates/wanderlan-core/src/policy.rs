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

use crate::config::{Mesh, Site, VlanMap};

/// The parts of a site that only its managers may change.
#[derive(Debug, PartialEq, Eq)]
struct Rules<'a> {
    address: std::net::Ipv4Addr,
    vlans: Option<String>,
    online: bool,
    managed_by: Option<&'a str>,
    self_managed: bool,
    maps: Vec<&'a VlanMap>,
}

fn rules<'a>(mesh: &'a Mesh, site: &'a Site) -> Rules<'a> {
    let mut maps: Vec<&VlanMap> = mesh.maps_of(&site.name).collect();
    maps.sort_by_key(|m| (m.local, m.shared));
    Rules {
        address: site.address,
        vlans: site.vlans.as_ref().map(ToString::to_string),
        online: site.online,
        managed_by: site.managed_by.as_deref(),
        self_managed: site.self_managed,
        maps,
    }
}

/// The manager of `site` if it is still part of the network.
pub fn manager<'a>(mesh: &Mesh, site: &'a Site) -> Option<&'a str> {
    site.managed_by
        .as_deref()
        .filter(|m| mesh.site(m).is_some())
}

/// The site that manages `editor` if `editor` is restricted (joined with an
/// invitation and not allowed to manage itself).
pub fn restricted_by<'a>(mesh: &'a Mesh, editor: &str) -> Option<&'a str> {
    let site = mesh.site(editor)?;
    if site.self_managed {
        return None;
    }
    manager(mesh, site)
}

/// True if `editor` may change the rules of `site` in `mesh`.
pub fn may_change(mesh: &Mesh, site: &Site, editor: &str) -> bool {
    match manager(mesh, site) {
        Some(m) => editor == m || (editor == site.name && site.self_managed),
        None => restricted_by(mesh, editor).is_none(),
    }
}

/// True if `editor` may change the VLANs of the network (`mesh.vlans`).
pub fn may_change_network_vlans(mesh: &Mesh, editor: &str) -> bool {
    restricted_by(mesh, editor).is_none()
}

/// Explains who may change `site`, for displays.
pub fn managers(mesh: &Mesh, site: &Site) -> String {
    match (manager(mesh, site), site.self_managed) {
        (None, _) => "every site that is not managed by another".into(),
        (Some(m), false) => format!("only \"{m}\""),
        (Some(m), true) => format!("\"{m}\" and \"{}\" itself", site.name),
    }
}

/// Short form for tables: `anyone`, `a`, `a+self`.
pub fn managed_label(mesh: &Mesh, site: &Site) -> String {
    match (manager(mesh, site), site.self_managed) {
        (None, _) => "anyone".into(),
        (Some(m), false) => m.to_string(),
        (Some(m), true) => format!("{m}+self"),
    }
}

/// Why `editor` may not `what` (change, remove) `site`; only meaningful if
/// it may not.
pub fn why_not(mesh: &Mesh, site: &Site, editor: &str, what: &str) -> String {
    let name = &site.name;
    if let Some(m) = restricted_by(mesh, editor)
        && manager(mesh, site) != Some(editor)
    {
        return format!(
            "this site (\"{editor}\") may not {what} site \"{name}\": {}",
            restricted_reason(m)
        );
    }
    format!(
        "this site (\"{editor}\") may not {what} site \"{name}\": its VLANs, mappings and online state are managed by {}; make the change there",
        managers(mesh, site)
    )
}

/// Why `editor` may not change the network's VLANs; empty if it may.
pub fn why_not_network(mesh: &Mesh, editor: &str) -> String {
    restricted_by(mesh, editor).map_or_else(String::new, |m| {
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
pub fn check_edit(old: &Mesh, new: &Mesh, editor: &str) -> Result<(), String> {
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
            online: true,
            managed_by: managed_by.map(Into::into),
            self_managed,
        }
    }

    fn mesh() -> Mesh {
        let mut mesh = Mesh::new(VlanSet::all());
        mesh.sites = vec![
            site("a", 1, None, false),
            site("b", 2, None, false),
            site("c", 3, Some("a"), false),
            site("d", 4, Some("a"), true),
        ];
        mesh
    }

    #[test]
    fn only_the_inviting_site_changes_an_invited_site() {
        let old = mesh();
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
        let old = mesh();
        let mut new = old.clone();
        new.sites[3].online = false;
        assert!(check_edit(&old, &new, "d").is_ok());
        assert!(check_edit(&old, &new, "a").is_ok());
        assert!(check_edit(&old, &new, "b").is_err());
    }

    #[test]
    fn mappings_removal_and_management_are_protected() {
        let old = mesh();
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
    }

    #[test]
    fn open_sites_and_network_settings_are_open_to_unrestricted_sites() {
        let old = mesh();
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
        let old = mesh();
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
        let mut old = mesh();
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
        let m = mesh();
        assert_eq!(managed_label(&m, &m.sites[0]), "anyone");
        assert_eq!(managed_label(&m, &m.sites[2]), "a");
        assert_eq!(managed_label(&m, &m.sites[3]), "a+self");
        assert!(why_not(&m, &m.sites[0], "c", "change").contains("make the change on \"a\""));
        assert!(why_not(&m, &m.sites[2], "b", "change").contains("only \"a\""));
        assert!(may_change_network_vlans(&m, "b"));
        assert!(!may_change_network_vlans(&m, "c"));
    }
}
