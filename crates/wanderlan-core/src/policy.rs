//! Who may change what.
//!
//! A site that joined with an invitation is managed by the site that created
//! the invitation (`managed_by`): only that site may change the rules of the
//! joined site, i.e. its VLANs, its VLAN mappings, whether it is online, who
//! manages it, and whether it stays in the network at all. With
//! `self_managed` the site may change its own rules too. Sites without
//! `managed_by` (the first site, sites added by hand) may be changed by every
//! site; so may sites whose manager is no longer part of the network.
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

/// True if `editor` may change the rules of `site` in `mesh`.
pub fn may_change(mesh: &Mesh, site: &Site, editor: &str) -> bool {
    match site.managed_by.as_deref() {
        None => true,
        // A manager that left the network can no longer decide.
        Some(manager) if mesh.site(manager).is_none() => true,
        Some(manager) => editor == manager || (editor == site.name && site.self_managed),
    }
}

/// Explains who may change `site`, for messages.
pub fn managers(site: &Site) -> String {
    match (site.managed_by.as_deref(), site.self_managed) {
        (None, _) => "every site".into(),
        (Some(m), false) => format!("only \"{m}\""),
        (Some(m), true) => format!("\"{m}\" and \"{}\" itself", site.name),
    }
}

/// Checks that `editor` only changes what it may: for every site of `old`
/// whose rules differ in `new` (or that is gone), `editor` must be allowed.
pub fn check_edit(old: &Mesh, new: &Mesh, editor: &str) -> Result<(), String> {
    for site in &old.sites {
        if may_change(old, site, editor) {
            continue;
        }
        let what = match new.site(&site.name) {
            None => "remove".to_string(),
            Some(changed) if rules(new, changed) != rules(old, site) => "change".to_string(),
            Some(_) => continue,
        };
        return Err(format!(
            "this site (\"{editor}\") may not {what} site \"{}\": its VLANs, mappings and online state are managed by {}; make the change there",
            site.name,
            managers(site)
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
    fn open_sites_and_network_settings_stay_open() {
        let old = mesh();
        let mut new = old.clone();
        new.sites[1].vlans = Some(VlanSet::from_range(30, 30));
        new.commit_timeout = std::time::Duration::from_secs(120);
        new.vlans = VlanSet::from_range(10, 50);
        assert!(check_edit(&old, &new, "c").is_ok());
        // Without its manager, an invited site is open again.
        let mut orphan = old.clone();
        orphan.sites.retain(|s| s.name != "a");
        let mut changed = orphan.clone();
        changed.sites[1].online = false;
        assert!(check_edit(&orphan, &changed, "b").is_ok());
    }
}
