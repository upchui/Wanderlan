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
//! A client changes only itself and the sites below it (its clients, their
//! clients, ...): for them it is the server. It never changes its own
//! server or sites beside it.
//!
//! A site may always rename itself. Another site may rename it only if it
//! is its manager (`managed_by`) and may change it; a client never renames
//! its server.
//!
//! Every wanderland checks this when a change is made on it (TUI, CLI,
//! edited files), and a server again when it takes over the changes of a
//! client: those of the client and of the sites below it, which come
//! through it. It is an agreement between the daemons, not a cryptographic
//! protection.

use std::collections::BTreeSet;

use crate::config::{Breakouts, Network, Site, VlanMap};
use crate::identity::SiteId;
use crate::topology;

/// The parts of a site that only its managers may change.
#[derive(Debug, PartialEq, Eq)]
struct Rules<'a> {
    address: std::net::Ipv4Addr,
    vlans: Option<String>,
    online: bool,
    managed_by: Option<SiteId>,
    self_managed: bool,
    uplink: Option<SiteId>,
    maps: Vec<&'a VlanMap>,
    /// The local breakout bypasses the central firewall: its manager
    /// decides.
    breakout: &'a Breakouts,
}

fn rules<'a>(network: &'a Network, site: &'a Site) -> Rules<'a> {
    let mut maps: Vec<&VlanMap> = network.maps_of(site.id).collect();
    maps.sort_by_key(|m| (m.local, m.shared));
    Rules {
        address: site.address,
        vlans: site.vlans.as_ref().map(ToString::to_string),
        online: site.online,
        managed_by: site.managed_by,
        self_managed: site.self_managed,
        uplink: site.uplink,
        maps,
        breakout: &site.breakout,
    }
}

/// The manager of `site` if it is still part of the network.
pub fn manager(network: &Network, site: &Site) -> Option<SiteId> {
    site.managed_by.filter(|m| network.site(*m).is_some())
}

/// The site that manages `editor` if `editor` is restricted (joined with an
/// invitation and not allowed to manage itself). A site that is managed by
/// itself ("only alex may change alex") is never restricted.
pub fn restricted_by(network: &Network, editor: SiteId) -> Option<SiteId> {
    let site = network.site(editor)?;
    if site.self_managed {
        return None;
    }
    manager(network, site).filter(|m| *m != editor)
}

/// True if `editor` may change the rules of `site` in `network`.
pub fn may_change(network: &Network, site: &Site, editor: SiteId) -> bool {
    // A client only changes itself and the sites below it, never its
    // server.
    if client_of(network, editor).is_some() && !itself_or_below(network, site.id, editor) {
        return false;
    }
    match manager(network, site) {
        Some(m) => editor == m || (editor == site.id && site.self_managed),
        None => restricted_by(network, editor).is_none(),
    }
}

/// True if `editor` may change the VLANs of the tunnel between `a` and `b`:
/// whoever may change one of the two sites. A site that is not there yet is
/// judged when it is added.
pub fn may_change_tunnel(network: &Network, a: SiteId, b: SiteId, editor: SiteId) -> bool {
    [a, b].iter().any(|id| {
        network
            .site(*id)
            .is_none_or(|site| may_change(network, site, editor))
    })
}

/// Why `editor` may not change the VLANs of the tunnel between `a` and `b`;
/// only meaningful if it may not.
pub fn why_not_tunnel(network: &Network, a: SiteId, b: SiteId, editor: SiteId) -> String {
    let me = network.label(editor);
    let what = format!(
        "the VLANs of the tunnel between \"{}\" and \"{}\"",
        network.label(a),
        network.label(b)
    );
    if let Some(m) = restricted_by(network, editor) {
        return format!(
            "this site (\"{me}\") may not change {what}: {}",
            restricted_reason(&network.label(m))
        );
    }
    if let Some(up) = client_of(network, editor) {
        return format!(
            "this site (\"{me}\") is a client of \"{}\": it may only change its own tunnels, not {what}",
            network.label(up)
        );
    }
    let by = |id: SiteId| {
        network
            .site(id)
            .map_or_else(String::new, |s| managers(network, s))
    };
    format!(
        "this site (\"{me}\") may not change {what}: \"{}\" is managed by {}, \"{}\" by {}",
        network.label(a),
        by(a),
        network.label(b),
        by(b)
    )
}

/// True if `site` is `editor` or hangs (indirectly) off it.
fn itself_or_below(network: &Network, site: SiteId, editor: SiteId) -> bool {
    site == editor || topology::is_below(network, site, editor)
}

/// The site `editor` is a client of, if it is a client.
pub fn client_of(network: &Network, editor: SiteId) -> Option<SiteId> {
    network.site(editor)?.uplink
}

/// True if `editor` may change the VLANs of the network (`network.vlans`).
pub fn may_change_network_vlans(network: &Network, editor: SiteId) -> bool {
    restricted_by(network, editor).is_none()
}

/// Explains who may change `site`, for displays.
pub fn managers(network: &Network, site: &Site) -> String {
    match (manager(network, site), site.self_managed) {
        (None, _) => "every site that is not managed by another".into(),
        (Some(m), false) => format!("only \"{}\"", network.label(m)),
        (Some(m), true) => format!(
            "\"{}\" and \"{}\" itself",
            network.label(m),
            network.label(site.id)
        ),
    }
}

/// Short form for tables: `nobody` (every server may change it), `a`,
/// `a+self`.
pub fn managed_label(network: &Network, site: &Site) -> String {
    match (manager(network, site), site.self_managed) {
        (None, _) => "nobody".into(),
        (Some(m), false) => network.label(m),
        (Some(m), true) => format!("{}+self", network.label(m)),
    }
}

/// Why `editor` may not `what` (change, remove) `site`; only meaningful if
/// it may not.
pub fn why_not(network: &Network, site: &Site, editor: SiteId, what: &str) -> String {
    let name = network.label(site.id);
    let me = network.label(editor);
    if let Some(up) = client_of(network, editor)
        && !itself_or_below(network, site.id, editor)
    {
        return format!(
            "this site (\"{me}\") is a client of \"{}\": it may only change itself and the sites below it, not {what} site \"{name}\"",
            network.label(up)
        );
    }
    if let Some(m) = restricted_by(network, editor)
        && manager(network, site) != Some(editor)
    {
        return format!(
            "this site (\"{me}\") may not {what} site \"{name}\": {}",
            restricted_reason(&network.label(m))
        );
    }
    format!(
        "this site (\"{me}\") may not {what} site \"{name}\": its VLANs, mappings, breakouts and online state are managed by {}; make the change there",
        managers(network, site)
    )
}

/// Why `editor` may not change the network's VLANs; empty if it may.
pub fn why_not_network(network: &Network, editor: SiteId) -> String {
    restricted_by(network, editor).map_or_else(String::new, |m| {
        format!(
            "this site (\"{}\") may not change the VLANs of the network: {}",
            network.label(editor),
            restricted_reason(&network.label(m))
        )
    })
}

fn restricted_reason(manager: &str) -> String {
    format!(
        "it is managed by \"{manager}\" and may not change VLANs, mappings, breakouts or the online state (except of sites it invited itself); make the change on \"{manager}\""
    )
}

/// True if `editor` may rename `site`: the site itself always; its manager
/// (`managed_by`) if it may change it, so a client never renames its server.
pub fn may_rename(network: &Network, site: SiteId, editor: SiteId) -> bool {
    site == editor
        || network
            .site(site)
            .is_some_and(|s| manager(network, s) == Some(editor) && may_change(network, s, editor))
}

/// Who may rename `site`, for displays: `"x" itself and "m"` or
/// `only "x" itself`.
pub fn renamers(network: &Network, site: &Site) -> String {
    let name = network.label(site.id);
    match manager(network, site).filter(|m| *m != site.id && may_change(network, site, *m)) {
        Some(m) => format!("\"{name}\" itself and its manager \"{}\"", network.label(m)),
        None => format!("only \"{name}\" itself"),
    }
}

/// Why `editor` may not rename `site`; only meaningful if it may not.
pub fn why_not_rename(network: &Network, site: SiteId, editor: SiteId) -> String {
    let name = network.label(site);
    let me = network.label(editor);
    let who = network.site(site).map_or_else(
        || format!("only \"{name}\" itself"),
        |s| renamers(network, s),
    );
    if let Some(up) = client_of(network, editor)
        && network.site(site).is_some()
        && !itself_or_below(network, site, editor)
    {
        return format!(
            "this site (\"{me}\") is a client of \"{}\": it may only rename itself and the sites below it, not site \"{name}\" ({who} may)",
            network.label(up)
        );
    }
    format!("this site (\"{me}\") may not rename site \"{name}\": {who} may")
}

/// Checks that `editor` only changes what it may: the identities of sites
/// (a claim, a box that joined again), renames, the network's VLANs, and
/// every site of `old` whose rules differ in `new` (or that is gone).
/// `editor` may be given by its identity in `old` or in `new` (it claims its
/// own).
pub fn check_edit(old: &Network, new: &Network, editor: SiteId) -> Result<(), String> {
    check_by(old, new, &[editor], None)
}

/// [`check_edit`] for the changes a client passes on to its uplink: they
/// were made by the client `from` itself or by a site below it, so a change
/// passes if one of them may make it. A client only changes itself and the
/// sites below it, so this never reaches beyond them; each site on the way
/// already checked the changes of the sites below it. The VLANs of the
/// network (the default of every site) only `from` decides with its own
/// rights; sites new below it bring their own ports.
pub fn check_relayed(old: &Network, new: &Network, from: SiteId) -> Result<(), String> {
    let mut editors = vec![from];
    editors.extend(topology::below(old, from));
    check_by(old, new, &editors, Some(from))
}

/// The editor an error about `site` is told for: the site itself if it is
/// one of them, otherwise the first (the site that made or passed on the
/// change).
fn speaker(editors: &[SiteId], site: SiteId) -> SiteId {
    editors
        .iter()
        .find(|e| **e == site)
        .or_else(|| editors.first())
        .copied()
        .unwrap_or(site)
}

/// [`check_edit`] for changes made by one of `editors` (the first made or
/// passed them on); `relayed` names the client that passed them on.
fn check_by(
    old: &Network,
    new: &Network,
    editors: &[SiteId],
    relayed: Option<SiteId>,
) -> Result<(), String> {
    // The identities first: the rest compares the network with the new
    // identities, the changed references to such a site are no changes.
    let (base, rekeys) = crate::rename::rekeyed(old, new);
    check_rekeys(old, new, &rekeys, editors)?;
    let now = |e: SiteId| {
        rekeys
            .iter()
            .find(|(from, _)| *from == e)
            .map_or(e, |(_, to)| *to)
    };
    let editors: Vec<SiteId> = editors.iter().map(|e| now(*e)).collect();
    let relayed = relayed.map(now);
    let renamed = crate::rename::renames(&base, new);
    for (id, _, _) in &renamed {
        if !editors.iter().any(|e| may_rename(&base, *id, *e)) {
            return Err(why_not_rename(&base, *id, speaker(&editors, *id)));
        }
    }
    // A rename is no change of the rules.
    let mut base = base;
    for (id, _, to) in &renamed {
        if let Some(site) = base.site_mut(*id) {
            site.name.clone_from(to);
        }
    }
    check_changes(&base, new, &editors, relayed)
}

/// What `editor` may not do to `site` (as it is in `old`) in `new`, if
/// anything.
fn check_site(old: &Network, new: &Network, site: &Site, editor: SiteId) -> Result<(), String> {
    let now = new.site(site.id);
    // A released site changes its VLANs, mappings and online state; who
    // manages it, where it is plugged in and its address stay the decision
    // of its manager.
    if editor == site.id
        && site.self_managed
        && let Some(m) = manager(old, site).filter(|m| *m != editor)
        && let Some(now) = now
    {
        let fixed = |s: &Site| (s.address, s.managed_by, s.uplink);
        if fixed(now) != fixed(site) {
            return Err(format!(
                "this site (\"{}\") may not change who manages it, its server or its address: that is for \"{}\"",
                old.label(editor),
                old.label(m)
            ));
        }
    }
    if may_change(old, site, editor) {
        return Ok(());
    }
    match now {
        None => Err(why_not(old, site, editor, "remove")),
        Some(now) if rules(new, now) != rules(old, site) => {
            Err(why_not(old, site, editor, "change"))
        }
        // Its own VLANs: the site itself, or whoever may change it.
        Some(now) if now.own_vlans != site.own_vlans && editor != site.id => {
            Err(why_not(old, site, editor, "change the own VLANs of"))
        }
        Some(_) => Ok(()),
    }
}

/// [`check_edit`] for two networks with the same sites and names.
fn check_changes(
    old: &Network,
    new: &Network,
    editors: &[SiteId],
    relayed: Option<SiteId>,
) -> Result<(), String> {
    let Some(&first) = editors.first() else {
        return Ok(());
    };
    // The default of every site: by the rights of the site that made or
    // passed on the change (a released site below a restricted one does
    // not open them).
    if old.vlans != new.vlans && !may_change_network_vlans(old, first) {
        return Err(why_not_network(old, first));
    }
    // The settings of `[network]` would change those of every other site
    // (where it has none of its own): each site sets its own.
    let settings = |n: &Network| {
        (
            n.control_port,
            n.tunnel_port,
            n.commit_timeout,
            n.mtu_probe_interval,
            n.mss,
        )
    };
    if settings(old) != settings(new) {
        return Err(
            "the ports, the commit timeout, the path MTU check and the MSS are set per site, each on itself (`wanderlan settings`, or Settings in the terminal UI); the values of [network] are only the defaults"
                .to_string(),
        );
    }
    // The ports of a site: only the site itself; a site new below the
    // client that passes the change on brings its own.
    for site in &new.sites {
        let before = old.site(site.id).map(|s| s.ports).unwrap_or_default();
        let brought = old.site(site.id).is_none()
            && relayed.is_some_and(|r| topology::is_below(new, site.id, r));
        if site.ports != before && !editors.contains(&site.id) && !brought {
            return Err(format!(
                "only \"{}\" itself may change its ports (`wanderlan settings` there)",
                new.label(site.id)
            ));
        }
    }
    for site in &old.sites {
        if !editors
            .iter()
            .any(|e| check_site(old, new, site, *e).is_ok())
        {
            return check_site(old, new, site, speaker(editors, site.id));
        }
    }
    // The VLANs of a tunnel: whoever may change one of its two sites.
    let pairs: BTreeSet<[SiteId; 2]> = old
        .tunnels
        .iter()
        .chain(new.tunnels.iter())
        .map(|t| t.sites)
        .collect();
    for [a, b] in pairs {
        if old.tunnel_limit(a, b) != new.tunnel_limit(a, b)
            && !editors.iter().any(|e| may_change_tunnel(old, a, b, *e))
        {
            return Err(why_not_tunnel(old, a, b, first));
        }
    }
    // A site below a client stays below it: only that client tells it where
    // it went, and it would no longer know it.
    if let Some((site, up)) = topology::stranded(old, new) {
        let (site, up) = (old.label(site), old.label(up));
        return Err(format!(
            "site \"{site}\" hangs off \"{up}\", a client: elsewhere \"{up}\" would no longer know it and could not tell it where it went. Move it below \"{up}\", make \"{up}\" a server first, or let \"{site}\" join again through its new server (`wanderlan reset` there, then `wanderlan join` with an invitation of that site)"
        ));
    }
    // A client only adds sites below it (its clients, theirs, ...).
    for added in new.sites.iter().filter(|s| old.site(s.id).is_none()) {
        let fits = editors
            .iter()
            .any(|e| client_of(old, *e).is_none() || topology::is_below(new, added.id, *e));
        if !fits && let Some(up) = client_of(old, first) {
            return Err(format!(
                "this site (\"{}\") is a client of \"{}\": it may only add clients below it (its own and theirs), not \"{}\"",
                old.label(first),
                old.label(up),
                new.label(added.id)
            ));
        }
    }
    Ok(())
}

/// The sites that get another identity ([`crate::rename::rekeys`]): a site
/// claims its own once (a site of an older version that had none), its
/// server passes the claim on. Another identity for a site with one comes
/// only with a join, decided by the site that invited it; when that is a
/// client, its server gets the change from it. Nobody removes an identity.
/// A site of an older version renamed is a rename.
fn check_rekeys(
    old: &Network,
    new: &Network,
    rekeys: &[(SiteId, SiteId)],
    editors: &[SiteId],
) -> Result<(), String> {
    for (from, to) in rekeys {
        let (Some(site), Some(now)) = (old.site(*from), new.site(*to)) else {
            continue;
        };
        let its_server = site.uplink.is_some_and(|up| editors.contains(&up));
        let itself = editors.contains(from) || editors.contains(to);
        let allowed = match (site.has_identity(), now.has_identity()) {
            (false, false) => editors
                .iter()
                .any(|e| may_rename(old, *from, *e) || e == to),
            (false, true) => itself || its_server,
            (true, true) => its_server,
            (true, false) => false,
        };
        if allowed {
            continue;
        }
        let speaker = old.label(speaker(editors, *from));
        return Err(if !site.has_identity() && !now.has_identity() {
            why_not_rename(old, *from, self::speaker(editors, *from))
        } else {
            format!(
                "this site (\"{speaker}\") may not change the identity of site \"{}\": it changes only when that box joins again with an invitation (`wanderlan reset` there, then `wanderlan join`)",
                old.label(*from)
            )
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;
    use crate::config::{LocalVid, VlanSet};

    fn id(name: &str) -> SiteId {
        SiteId::provisional(name)
    }

    fn site(name: &str, last: u8, managed_by: Option<&str>, self_managed: bool) -> Site {
        Site {
            managed_by: managed_by.map(id),
            self_managed,
            ..Site::new(name, Ipv4Addr::new(10, 0, 0, last))
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
        assert!(check_edit(&old, &new, id("a")).is_ok());
        let err = check_edit(&old, &new, id("b")).unwrap_err();
        assert!(err.contains("may not change site \"c\""), "{err}");
        assert!(err.contains("only \"a\""), "{err}");
        assert!(check_edit(&old, &new, id("c")).is_err(), "not self-managed");
    }

    #[test]
    fn a_site_sets_its_own_ports_and_no_one_the_network_settings() {
        let old = network();
        let mut new = old.clone();
        new.sites[1].ports.control = Some(52821);
        assert!(check_edit(&old, &new, id("b")).is_ok());
        let err = check_edit(&old, &new, id("a")).unwrap_err();
        assert!(err.contains("only \"b\" itself"), "{err}");
        let mut new = old.clone();
        new.control_port = 52821;
        assert!(check_edit(&old, &new, id("a")).is_err());
        let mut new = old.clone();
        new.mss = Some(1300);
        assert!(check_edit(&old, &new, id("a")).is_err());
    }

    #[test]
    fn a_self_managed_site_changes_itself() {
        let old = network();
        let mut new = old.clone();
        new.sites[3].online = false;
        assert!(check_edit(&old, &new, id("d")).is_ok());
        assert!(check_edit(&old, &new, id("a")).is_ok());
        assert!(check_edit(&old, &new, id("b")).is_err());
        // Who manages it and where it is plugged in stay its manager's.
        for change in [
            |s: &mut Site| s.managed_by = Some(id("d")),
            |s: &mut Site| s.uplink = Some(id("b")),
        ] {
            let mut new = old.clone();
            change(&mut new.sites[3]);
            let err = check_edit(&old, &new, id("d")).unwrap_err();
            assert!(err.contains("that is for \"a\""), "{err}");
        }
    }

    #[test]
    fn mappings_removal_and_management_are_protected() {
        let old = network();
        let mut new = old.clone();
        new.maps.push(VlanMap {
            site: id("c"),
            local: LocalVid::Native,
            shared: 20,
        });
        assert!(check_edit(&old, &new, id("b")).is_err());
        let mut gone = old.clone();
        gone.sites.retain(|s| s.name != "c");
        assert!(
            check_edit(&old, &gone, id("b"))
                .unwrap_err()
                .contains("may not remove")
        );
        let mut taken = old.clone();
        taken.sites[2].managed_by = Some(id("b"));
        assert!(check_edit(&old, &taken, id("b")).is_err());
        // Which site it is plugged into is a rule too.
        let mut moved = old.clone();
        moved.sites[2].uplink = Some(id("b"));
        assert!(check_edit(&old, &moved, id("b")).is_err());
        assert!(check_edit(&old, &moved, id("a")).is_ok());
    }

    #[test]
    fn open_sites_and_network_settings_are_open_to_unrestricted_sites() {
        let old = network();
        let mut new = old.clone();
        new.sites[1].vlans = Some(VlanSet::from_range(30, 30));
        new.vlans = VlanSet::from_range(10, 50);
        assert!(check_edit(&old, &new, id("a")).is_ok());
        assert!(check_edit(&old, &new, id("b")).is_ok());
        assert!(
            check_edit(&old, &new, id("d")).is_ok(),
            "released: like its manager"
        );
        // Without its manager, an invited site is open again.
        let mut orphan = old.clone();
        orphan.sites.retain(|s| s.name != "a");
        let mut changed = orphan.clone();
        changed.sites[1].online = false;
        assert!(check_edit(&orphan, &changed, id("b")).is_ok());
        assert!(
            check_edit(&orphan, &changed, id("c")).is_ok(),
            "no longer restricted"
        );
    }

    #[test]
    fn a_restricted_site_changes_no_vlan_rules() {
        let old = network();
        assert_eq!(restricted_by(&old, id("c")), Some(id("a")));
        assert_eq!(restricted_by(&old, id("d")), None, "released");
        assert_eq!(restricted_by(&old, id("a")), None);
        // The network's VLANs.
        let mut new = old.clone();
        new.vlans = VlanSet::from_range(10, 50);
        let err = check_edit(&old, &new, id("c")).unwrap_err();
        assert!(err.contains("managed by \"a\""), "{err}");
        // An open site (the inviting one).
        let mut new = old.clone();
        new.sites[0].vlans = Some(VlanSet::from_range(10, 20));
        assert!(check_edit(&old, &new, id("c")).is_err());
        // A mapping of an open site.
        let mut new = old.clone();
        new.maps.push(VlanMap {
            site: id("b"),
            local: LocalVid::Native,
            shared: 20,
        });
        assert!(check_edit(&old, &new, id("c")).is_err());
        // Its own ports are its own business.
        let mut new = old.clone();
        new.sites[2].ports.tunnel = Some(52833);
        assert!(check_edit(&old, &new, id("c")).is_ok());
    }

    #[test]
    fn a_restricted_site_manages_the_sites_it_invited() {
        let mut old = network();
        old.sites.push(site("e", 5, Some("c"), false));
        let mut new = old.clone();
        new.sites[4].online = false;
        assert!(check_edit(&old, &new, id("c")).is_ok());
        assert!(
            check_edit(&old, &new, id("a")).is_err(),
            "e is managed by c only"
        );
    }

    #[test]
    fn labels_and_messages() {
        let m = network();
        assert_eq!(managed_label(&m, &m.sites[0]), "nobody");
        assert_eq!(managed_label(&m, &m.sites[2]), "a");
        assert_eq!(managed_label(&m, &m.sites[3]), "a+self");
        assert!(why_not(&m, &m.sites[0], id("c"), "change").contains("make the change on \"a\""));
        assert!(why_not(&m, &m.sites[2], id("b"), "change").contains("only \"a\""));
        assert!(may_change_network_vlans(&m, id("b")));
        assert!(!may_change_network_vlans(&m, id("c")));
    }

    #[test]
    fn a_client_changes_only_itself_and_the_sites_below_it() {
        let mut old = network();
        // d is a client of a (released, so its VLANs are its own), e a client of d.
        old.sites[3].uplink = Some(id("a"));
        old.sites.push(Site {
            uplink: Some(id("d")),
            managed_by: Some(id("d")),
            ..site("e", 5, None, false)
        });
        let mut new = old.clone();
        new.sites[3].online = false;
        new.sites[4].online = false;
        assert!(
            check_edit(&old, &new, id("d")).is_ok(),
            "itself and its client"
        );
        // g hangs off e: d is a server for it too (and manages it).
        let mut deep = old.clone();
        deep.sites.push(Site {
            uplink: Some(id("e")),
            managed_by: Some(id("d")),
            ..site("g", 7, None, false)
        });
        let mut changed = deep.clone();
        changed.sites[5].online = false;
        assert!(
            check_edit(&deep, &changed, id("d")).is_ok(),
            "a site below it"
        );
        changed.sites[5].managed_by = Some(id("e"));
        let mut by_e = changed.clone();
        by_e.sites[5].online = true;
        assert!(
            check_edit(&changed, &by_e, id("d")).is_err(),
            "managed by e"
        );
        // Up from e to d: e would no longer know it.
        let mut moved = deep.clone();
        moved.sites[5].uplink = Some(id("d"));
        let err = check_edit(&deep, &moved, id("d")).unwrap_err();
        assert!(err.contains("hangs off \"e\", a client"), "{err}");
        // With e a server, e knows where g went, but d would lose e; with
        // d a server too, nobody is lost.
        moved.sites[4].uplink = None;
        assert_eq!(topology::stranded(&deep, &moved), Some((id("e"), id("d"))));
        moved.sites[3].uplink = None;
        assert_eq!(topology::stranded(&deep, &moved), None);
        let mut server = old.clone();
        server.sites[1].online = false;
        let err = check_edit(&old, &server, id("d")).unwrap_err();
        assert!(err.contains("is a client of \"a\""), "{err}");
        let mut added = old.clone();
        added.sites.push(site("f", 6, None, false));
        assert!(
            check_edit(&old, &added, id("d"))
                .unwrap_err()
                .contains("only add clients")
        );
        added.sites[5].uplink = Some(id("d"));
        assert!(check_edit(&old, &added, id("d")).is_ok());
        added.sites[5].uplink = Some(id("e"));
        assert!(
            check_edit(&old, &added, id("d")).is_ok(),
            "below its client"
        );
    }

    #[test]
    fn a_restricted_site_sets_its_own_vlans() {
        let old = network();
        assert_eq!(restricted_by(&old, id("c")), Some(id("a")));
        let mut new = old.clone();
        new.sites[2].own_vlans = Some(VlanSet::from_range(60, 60));
        assert!(check_edit(&old, &new, id("c")).is_ok(), "c itself");
        assert!(check_edit(&old, &new, id("a")).is_ok(), "its manager");
        let err = check_edit(&old, &new, id("b")).unwrap_err();
        assert!(
            err.contains("may not change the own VLANs of site \"c\""),
            "{err}"
        );
    }

    #[test]
    fn a_site_managed_by_itself_is_not_restricted() {
        let mut old = network();
        old.sites[0].managed_by = Some(id("a"));
        assert_eq!(restricted_by(&old, id("a")), None);
        assert!(may_change_network_vlans(&old, id("a")));
        let mut new = old.clone();
        new.vlans = VlanSet::from_range(10, 20);
        new.sites[0].vlans = Some(VlanSet::from_range(10, 10));
        new.sites[2].online = false;
        assert!(
            check_edit(&old, &new, id("a")).is_ok(),
            "network, itself, its clients"
        );
        // Others still may not change it.
        let mut other = old.clone();
        other.sites[0].online = false;
        assert!(check_edit(&old, &other, id("b")).is_err());
    }

    #[test]
    fn a_tunnel_is_changed_by_whoever_may_change_one_of_its_sites() {
        let old = network();
        let mut new = old.clone();
        new.set_tunnel_limit(id("b"), id("c"), Some(VlanSet::from_range(10, 20)));
        assert!(check_edit(&old, &new, id("b")).is_ok(), "b may change b");
        assert!(check_edit(&old, &new, id("a")).is_ok(), "a manages c");
        let err = check_edit(&old, &new, id("c")).unwrap_err();
        assert!(
            err.contains("may not change the VLANs of the tunnel between \"b\" and \"c\""),
            "{err}"
        );
        assert!(err.contains("managed by \"a\""), "{err}");
        let mut own = old.clone();
        own.set_tunnel_limit(id("a"), id("d"), Some(VlanSet::new()));
        assert!(
            check_edit(&old, &own, id("d")).is_ok(),
            "d may change itself"
        );
    }

    #[test]
    fn a_site_renames_itself_its_manager_renames_it_a_client_never_its_server() {
        let mut old = network();
        // c (restricted) is a client of a and has a client e of its own.
        old.sites[2].uplink = Some(id("a"));
        old.sites.push(Site {
            uplink: Some(id("c")),
            managed_by: Some(id("c")),
            ..site("e", 5, None, false)
        });
        let mut renamed = old.clone();
        renamed.rename_site(id("c"), "c2");
        assert!(
            check_edit(&old, &renamed, id("c")).is_ok(),
            "its name is its own, also when restricted"
        );
        assert!(check_edit(&old, &renamed, id("a")).is_ok(), "its manager");
        let err = check_edit(&old, &renamed, id("b")).unwrap_err();
        assert!(
            err.contains("\"c\" itself and its manager \"a\" may"),
            "{err}"
        );
        // c manages its own client e.
        let mut client = old.clone();
        client.rename_site(id("e"), "e2");
        assert!(check_edit(&old, &client, id("c")).is_ok());
        // A server without a manager: only itself.
        let mut server = old.clone();
        server.rename_site(id("b"), "b2");
        assert!(check_edit(&old, &server, id("b")).is_ok());
        let err = check_edit(&old, &server, id("a")).unwrap_err();
        assert!(err.contains("only \"b\" itself may"), "{err}");
        // A client never renames its server, not even as its manager.
        let mut odd = old.clone();
        odd.sites[0].managed_by = Some(id("c"));
        let mut renamed = odd.clone();
        renamed.rename_site(id("a"), "a2");
        let err = check_edit(&odd, &renamed, id("c")).unwrap_err();
        assert!(err.contains("is a client of \"a\""), "{err}");
        // A rename and another change of the site at once: the change is
        // judged as always.
        let mut both = old.clone();
        both.rename_site(id("c"), "c2");
        both.sites[2].vlans = Some(VlanSet::from_range(10, 20));
        assert!(check_edit(&old, &both, id("c")).is_err(), "c is restricted");
    }

    #[test]
    fn a_site_claims_its_identity_its_server_passes_it_on() {
        let real = |n: u8| SiteId::from_bytes([n; 32]);
        let mut old = network();
        // e is a client of c, c a client of a.
        old.sites.push(Site {
            uplink: Some(id("c")),
            managed_by: Some(id("c")),
            ..site("e", 5, None, false)
        });
        old.sites[2].uplink = Some(id("a"));
        let mut claimed = old.clone();
        claimed.rekey(id("c"), real(3));
        assert!(
            check_edit(&old, &claimed, id("c")).is_ok(),
            "its own, also restricted"
        );
        assert!(
            check_edit(&old, &claimed, real(3)).is_ok(),
            "by its new identity"
        );
        assert!(
            check_edit(&old, &claimed, id("a")).is_ok(),
            "its server passes it on"
        );
        let err = check_edit(&old, &claimed, id("b")).unwrap_err();
        assert!(
            err.contains("may not change the identity of site \"c\""),
            "{err}"
        );
        // Another identity for a known site: only by the site that invited
        // it, through a join.
        let mut other = claimed.clone();
        other.rekey(real(3), real(9));
        assert!(check_edit(&claimed, &other, real(3)).is_err());
        assert!(check_edit(&claimed, &other, id("b")).is_err());
        assert!(check_edit(&claimed, &other, id("a")).is_ok());
        let mut client = claimed.clone();
        client.rekey(id("e"), real(5));
        let mut took_over = client.clone();
        took_over.rekey(real(5), real(6));
        assert!(
            check_edit(&client, &took_over, real(3)).is_ok(),
            "c invited e"
        );
        assert!(check_edit(&client, &took_over, real(5)).is_err());
        // Nobody removes an identity.
        let mut gone = claimed.clone();
        gone.rekey(real(3), id("c"));
        assert!(check_edit(&claimed, &gone, real(3)).is_err());
        assert!(check_edit(&claimed, &gone, id("a")).is_err());
        // A rename keeps the identity and is no identity change.
        let mut renamed = claimed.clone();
        renamed.rename_site(real(3), "c2");
        assert!(check_edit(&claimed, &renamed, real(3)).is_ok());
    }

    #[test]
    fn two_sites_with_the_same_name_have_their_own_rights() {
        let real = |n: u8| SiteId::from_bytes([n; 32]);
        let mut old = network();
        old.rekey(id("a"), real(1));
        old.rekey(id("b"), real(2));
        // Two clients called "x": one managed by a, one by b.
        for (n, by, last) in [(7, real(1), 7), (8, real(2), 8)] {
            old.sites.push(Site {
                id: real(n),
                uplink: Some(by),
                managed_by: Some(by),
                ..site("x", last, None, false)
            });
        }
        let mut new = old.clone();
        new.site_mut(real(8)).unwrap().online = false;
        assert!(check_edit(&old, &new, real(2)).is_ok());
        let err = check_edit(&old, &new, real(1)).unwrap_err();
        assert!(
            err.contains(&format!("site \"x#{}\"", real(8).fingerprint())),
            "{err}"
        );
    }
}
