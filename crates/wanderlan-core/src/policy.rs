//! Who may change what.
//!
//! The tunnel between a client and its server belongs to the two of them:
//! the option `changes` of the client says who changes it, **only its
//! server**, **only the client** or **both**. That is what goes through the
//! tunnel: the client's VLANs and VLAN mappings, the VLANs of that tunnel,
//! its breakouts and whether it is online; and the option itself, for
//! whoever may change the tunnel now (with "only the client" its server has
//! no say until the client gives it back). The address of the client, its
//! server (moving it, making it a server) and whether it stays in the
//! network are always for its server.
//!
//! "Its server" is always the other end of that tunnel: a client + server
//! is the server of its own clients, the sites above it have no say there.
//! A client changes nothing above it.
//!
//! A server has no such option: every server changes it, and the tunnels
//! between servers.
//!
//! A client that may not change its own tunnel ("only the server") is
//! **restricted**: it does not change the VLANs of the network either.
//!
//! A site may always rename itself; its server may rename it too, a client
//! never renames its server. Its ports only the site itself changes.
//!
//! Every wanderland checks this when a change is made on it (TUI, CLI,
//! edited files), and a server again when it takes over the changes of a
//! client: those of the client and of the sites below it, which come
//! through it. It is an agreement between the daemons, not a cryptographic
//! protection.

use std::collections::BTreeSet;

use crate::config::{Breakouts, Changes, Network, Site, VlanMap};
use crate::identity::SiteId;
use crate::topology;

/// What goes through the tunnel of a client to its server: changed by whom
/// its option `changes` names (a server: by every server).
#[derive(Debug, PartialEq, Eq)]
struct Rules<'a> {
    vlans: Option<String>,
    online: bool,
    /// Only a client has the option.
    changes: Option<Changes>,
    maps: Vec<&'a VlanMap>,
    /// The local breakout bypasses the central firewall.
    breakout: &'a Breakouts,
}

fn rules<'a>(network: &'a Network, site: &'a Site) -> Rules<'a> {
    let mut maps: Vec<&VlanMap> = network.maps_of(site.id).collect();
    maps.sort_by_key(|m| (m.local, m.shared));
    Rules {
        vlans: site.vlans.as_ref().map(ToString::to_string),
        online: site.online,
        changes: site.uplink.map(|_| site.changes),
        maps,
        breakout: &site.breakout,
    }
}

/// The server of `site` if it is a client: the other end of its tunnel.
pub fn server_of(network: &Network, site: &Site) -> Option<SiteId> {
    site.uplink.filter(|up| network.site(*up).is_some())
}

/// The server of `editor` if `editor` is restricted: a client that may not
/// change its own tunnel ("only the server").
pub fn restricted_by(network: &Network, editor: SiteId) -> Option<SiteId> {
    let site = network.site(editor)?;
    server_of(network, site).filter(|_| !site.changes.client())
}

/// True if `editor` may change the tunnel of `site` to its server (its
/// VLANs, mappings, breakouts, online state, the option `changes`); a
/// server: every server.
pub fn may_change(network: &Network, site: &Site, editor: SiteId) -> bool {
    match server_of(network, site) {
        Some(server) => {
            (editor == server && site.changes.server())
                || (editor == site.id && site.changes.client())
        }
        None => client_of(network, editor).is_none(),
    }
}

/// True if `editor` may change the address or the server of `site`, or
/// remove it: its server; a server every server.
pub fn may_manage(network: &Network, site: &Site, editor: SiteId) -> bool {
    match server_of(network, site) {
        Some(server) => editor == server,
        None => client_of(network, editor).is_none(),
    }
}

/// The client of the tunnel between `a` and `b`, if one is the server of
/// the other.
fn client_in(network: &Network, a: SiteId, b: SiteId) -> Option<&Site> {
    [(a, b), (b, a)]
        .into_iter()
        .filter_map(|(c, s)| network.site(c).filter(|c| c.uplink == Some(s)))
        .next()
}

/// True if `editor` may change the VLANs of the tunnel between `a` and `b`:
/// between a client and its server as the client's option says, between
/// servers every server. A site that is not there yet is judged when it is
/// added.
pub fn may_change_tunnel(network: &Network, a: SiteId, b: SiteId, editor: SiteId) -> bool {
    if let Some(client) = client_in(network, a, b) {
        return may_change(network, client, editor);
    }
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
    match client_in(network, a, b) {
        Some(client) => format!(
            "this site (\"{me}\") may not change {what}: it is changed by {}",
            managers(network, client)
        ),
        None => match client_of(network, editor) {
            Some(up) => format!(
                "this site (\"{me}\") is a client of \"{}\": it may only change its own tunnels, not {what}",
                network.label(up)
            ),
            None => format!("this site (\"{me}\") may not change {what}"),
        },
    }
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

/// Who changes the tunnel of `site`, for displays: `only its server "hq"`,
/// `only "office" itself`, `its server "hq" and "office" itself`; for a
/// server `every server`.
pub fn managers(network: &Network, site: &Site) -> String {
    let name = network.label(site.id);
    match server_of(network, site) {
        None => "every server".into(),
        Some(server) => {
            let server = network.label(server);
            match site.changes {
                Changes::Server => format!("only its server \"{server}\""),
                Changes::Client => format!("only \"{name}\" itself"),
                Changes::Both => format!("its server \"{server}\" and \"{name}\" itself"),
            }
        }
    }
}

/// Short form for tables: `server`, `client` or `both`; `-` for a server.
pub fn managed_label(network: &Network, site: &Site) -> String {
    match server_of(network, site) {
        None => "-".into(),
        Some(_) => site.changes.as_str().into(),
    }
}

/// Why `editor` may not `what` (e.g. `change`) the tunnel of `site`; only
/// meaningful if it may not.
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
    let Some(server) = server_of(network, site) else {
        return format!("this site (\"{me}\") may not {what} site \"{name}\": every server may");
    };
    let on = if site.changes.server() {
        server
    } else {
        site.id
    };
    format!(
        "this site (\"{me}\") may not {what} site \"{name}\": its VLANs, mappings, breakouts and online state are changed by {} (Changes: {}); make the change on \"{}\"",
        managers(network, site),
        site.changes.label(),
        network.label(on)
    )
}

/// Why `editor` may not `what` (remove, move, readdress) `site`; only
/// meaningful if it may not ([`may_manage`]).
pub fn why_not_manage(network: &Network, site: &Site, editor: SiteId, what: &str) -> String {
    let name = network.label(site.id);
    let me = network.label(editor);
    match (server_of(network, site), client_of(network, editor)) {
        (Some(server), _) => format!(
            "this site (\"{me}\") may not {what} site \"{name}\": that is for its server \"{}\"",
            network.label(server)
        ),
        (None, Some(up)) => format!(
            "this site (\"{me}\") is a client of \"{}\": it may not {what} site \"{name}\"",
            network.label(up)
        ),
        (None, None) => format!("this site (\"{me}\") may not {what} site \"{name}\""),
    }
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

fn restricted_reason(server: &str) -> String {
    format!(
        "only its server \"{server}\" changes its tunnel (Changes: only the server), and it does not change the VLANs of the network either; make the change on \"{server}\""
    )
}

/// True if `editor` may rename `site`: the site itself always, its server
/// too; so a client never renames its server.
pub fn may_rename(network: &Network, site: SiteId, editor: SiteId) -> bool {
    site == editor
        || network
            .site(site)
            .is_some_and(|s| server_of(network, s) == Some(editor))
}

/// Who may rename `site`, for displays: `"x" itself and its server "s"` or
/// `only "x" itself`.
pub fn renamers(network: &Network, site: &Site) -> String {
    let name = network.label(site.id);
    match server_of(network, site) {
        Some(s) => format!("\"{name}\" itself and its server \"{}\"", network.label(s)),
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
    let Some(now) = new.site(site.id) else {
        return if may_manage(old, site, editor) {
            Ok(())
        } else {
            Err(why_not_manage(old, site, editor, "remove"))
        };
    };
    // Where it is: for its server.
    if (now.address, now.uplink) != (site.address, site.uplink) && !may_manage(old, site, editor) {
        return Err(why_not_manage(
            old,
            site,
            editor,
            "change the address or the server of",
        ));
    }
    if rules(new, now) != rules(old, site) && !may_change(old, site, editor) {
        return Err(why_not(old, site, editor, "change"));
    }
    // Its own VLANs: the site itself, or whoever may change its tunnel.
    if now.own_vlans != site.own_vlans && editor != site.id && !may_change(old, site, editor) {
        return Err(why_not(old, site, editor, "change the own VLANs of"));
    }
    Ok(())
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

    /// A site; with `server` a client of it whose tunnel `changes` decides.
    fn site(name: &str, last: u8, server: Option<&str>, changes: Changes) -> Site {
        Site {
            uplink: server.map(id),
            changes,
            ..Site::new(name, Ipv4Addr::new(10, 0, 0, last))
        }
    }

    /// Servers a and b; c and d clients of a: c changed only by a, d by
    /// both.
    fn network() -> Network {
        let mut network = Network::new(VlanSet::all());
        network.sites = vec![
            site("a", 1, None, Changes::Both),
            site("b", 2, None, Changes::Both),
            site("c", 3, Some("a"), Changes::Server),
            site("d", 4, Some("a"), Changes::Both),
        ];
        network
    }

    #[test]
    fn the_option_says_who_changes_the_tunnel_of_a_client() {
        let old = network();
        // Only the server.
        let mut new = old.clone();
        new.sites[2].vlans = Some(VlanSet::from_range(10, 20));
        assert!(check_edit(&old, &new, id("a")).is_ok());
        let err = check_edit(&old, &new, id("c")).unwrap_err();
        assert!(err.contains("only its server \"a\""), "{err}");
        let err = check_edit(&old, &new, id("b")).unwrap_err();
        assert!(err.contains("may not change site \"c\""), "{err}");
        // Both.
        let mut new = old.clone();
        new.sites[3].online = false;
        new.maps.push(VlanMap {
            site: id("d"),
            local: LocalVid::Native,
            shared: 20,
        });
        assert!(check_edit(&old, &new, id("a")).is_ok());
        assert!(check_edit(&old, &new, id("d")).is_ok());
        assert!(check_edit(&old, &new, id("b")).is_err(), "not its server");
        // Only the client: its server has no say, not even about the
        // option, until the client gives it back.
        let mut own = old.clone();
        own.sites[2].changes = Changes::Client;
        assert_eq!(restricted_by(&own, id("c")), None);
        let mut new = own.clone();
        new.sites[2].online = false;
        assert!(check_edit(&own, &new, id("c")).is_ok());
        assert!(check_edit(&own, &new, id("a")).is_err());
        let mut back = own.clone();
        back.sites[2].changes = Changes::Server;
        assert!(check_edit(&own, &back, id("a")).is_err());
        assert!(check_edit(&own, &back, id("c")).is_ok());
        // Its address, its server and whether it stays: its server.
        for change in [
            |s: &mut Site| s.address = Ipv4Addr::new(10, 0, 0, 9),
            |s: &mut Site| s.uplink = Some(id("b")),
        ] {
            let mut new = own.clone();
            change(&mut new.sites[2]);
            let err = check_edit(&own, &new, id("c")).unwrap_err();
            assert!(err.contains("that is for its server \"a\""), "{err}");
            assert!(check_edit(&own, &new, id("a")).is_ok());
        }
        let mut gone = own.clone();
        gone.sites.retain(|s| s.name != "c");
        assert!(check_edit(&own, &gone, id("a")).is_ok());
        let err = check_edit(&own, &gone, id("b")).unwrap_err();
        assert!(err.contains("may not remove"), "{err}");
        // A server: every server, no client.
        let mut new = old.clone();
        new.sites[1].vlans = Some(VlanSet::from_range(30, 30));
        assert!(check_edit(&old, &new, id("a")).is_ok());
        assert!(check_edit(&old, &new, id("b")).is_ok());
        let err = check_edit(&old, &new, id("d")).unwrap_err();
        assert!(err.contains("is a client of \"a\""), "{err}");
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
    fn a_restricted_site_changes_no_vlan_rules() {
        let old = network();
        assert_eq!(restricted_by(&old, id("c")), Some(id("a")));
        assert_eq!(restricted_by(&old, id("d")), None, "both");
        assert_eq!(restricted_by(&old, id("a")), None);
        // The network's VLANs.
        let mut new = old.clone();
        new.vlans = VlanSet::from_range(10, 50);
        let err = check_edit(&old, &new, id("c")).unwrap_err();
        assert!(err.contains("only its server \"a\""), "{err}");
        assert!(check_edit(&old, &new, id("d")).is_ok());
        // Its server.
        let mut new = old.clone();
        new.sites[0].vlans = Some(VlanSet::from_range(10, 20));
        assert!(check_edit(&old, &new, id("c")).is_err());
        // Its own ports and its own VLANs (towards its own clients) are its
        // own business.
        let mut new = old.clone();
        new.sites[2].ports.tunnel = Some(52833);
        new.sites[2].own_vlans = Some(VlanSet::from_range(60, 60));
        assert!(check_edit(&old, &new, id("c")).is_ok());
        let mut own = old.clone();
        own.sites[2].own_vlans = Some(VlanSet::from_range(60, 60));
        assert!(check_edit(&old, &own, id("a")).is_ok(), "its server");
        let err = check_edit(&old, &own, id("b")).unwrap_err();
        assert!(
            err.contains("may not change the own VLANs of site \"c\""),
            "{err}"
        );
    }

    #[test]
    fn labels_and_messages() {
        let m = network();
        assert_eq!(managed_label(&m, &m.sites[0]), "-");
        assert_eq!(managed_label(&m, &m.sites[2]), "server");
        assert_eq!(managed_label(&m, &m.sites[3]), "both");
        assert_eq!(
            managers(&m, &m.sites[3]),
            "its server \"a\" and \"d\" itself"
        );
        assert!(why_not(&m, &m.sites[2], id("b"), "change").contains("make the change on \"a\""));
        assert!(may_change_network_vlans(&m, id("b")));
        assert!(!may_change_network_vlans(&m, id("c")));
    }

    #[test]
    fn a_client_changes_only_itself_and_its_own_clients() {
        let mut old = network();
        // e is a client of d (only d changes it).
        old.sites.push(site("e", 5, Some("d"), Changes::Server));
        let mut new = old.clone();
        new.sites[3].online = false;
        new.sites[4].online = false;
        assert!(
            check_edit(&old, &new, id("d")).is_ok(),
            "itself and its client"
        );
        let mut e = old.clone();
        e.sites[4].online = false;
        assert!(check_edit(&old, &e, id("a")).is_err(), "not e's server");
        // g hangs off e: e is its server, not d.
        let mut deep = old.clone();
        deep.sites.push(site("g", 7, Some("e"), Changes::Server));
        let mut changed = deep.clone();
        changed.sites[5].online = false;
        assert!(check_edit(&deep, &changed, id("e")).is_ok());
        assert!(check_edit(&deep, &changed, id("d")).is_err());
        // Up from e to d: e would no longer know it.
        let mut moved = deep.clone();
        moved.sites[5].uplink = Some(id("d"));
        let err = check_edit(&deep, &moved, id("e")).unwrap_err();
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
        added.sites.push(site("f", 6, None, Changes::Both));
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
    fn a_tunnel_is_changed_as_the_option_of_its_client_says() {
        let old = network();
        let mut new = old.clone();
        new.set_tunnel_limit(id("a"), id("c"), Some(VlanSet::from_range(10, 20)));
        assert!(check_edit(&old, &new, id("a")).is_ok(), "its server");
        let err = check_edit(&old, &new, id("c")).unwrap_err();
        assert!(
            err.contains("may not change the VLANs of the tunnel between \"a\" and \"c\""),
            "{err}"
        );
        assert!(err.contains("only its server \"a\""), "{err}");
        assert!(check_edit(&old, &new, id("b")).is_err());
        let mut own = old.clone();
        own.set_tunnel_limit(id("a"), id("d"), Some(VlanSet::new()));
        assert!(check_edit(&old, &own, id("d")).is_ok(), "both");
        // Between servers: every server.
        let mut servers = old.clone();
        servers.set_tunnel_limit(id("a"), id("b"), Some(VlanSet::from_range(10, 20)));
        assert!(check_edit(&old, &servers, id("a")).is_ok());
        assert!(check_edit(&old, &servers, id("b")).is_ok());
        assert!(check_edit(&old, &servers, id("d")).is_err());
    }

    #[test]
    fn a_site_renames_itself_its_server_renames_it_a_client_never_its_server() {
        let mut old = network();
        // c (restricted) has a client e of its own.
        old.sites.push(site("e", 5, Some("c"), Changes::Server));
        let mut renamed = old.clone();
        renamed.rename_site(id("c"), "c2");
        assert!(
            check_edit(&old, &renamed, id("c")).is_ok(),
            "its name is its own, also when restricted"
        );
        assert!(check_edit(&old, &renamed, id("a")).is_ok(), "its server");
        let err = check_edit(&old, &renamed, id("b")).unwrap_err();
        assert!(
            err.contains("\"c\" itself and its server \"a\" may"),
            "{err}"
        );
        let mut client = old.clone();
        client.rename_site(id("e"), "e2");
        assert!(check_edit(&old, &client, id("c")).is_ok());
        // A server: only itself.
        let mut server = old.clone();
        server.rename_site(id("b"), "b2");
        assert!(check_edit(&old, &server, id("b")).is_ok());
        let err = check_edit(&old, &server, id("a")).unwrap_err();
        assert!(err.contains("only \"b\" itself may"), "{err}");
        // A client never renames its server.
        let mut up = old.clone();
        up.rename_site(id("a"), "a2");
        let err = check_edit(&old, &up, id("c")).unwrap_err();
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
        old.sites.push(site("e", 5, Some("c"), Changes::Server));
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
        // Another identity for a known site: only by its server, through a
        // join.
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
        // Two clients called "x": one of a, one of b.
        for (n, by, last) in [(7, real(1), 7), (8, real(2), 8)] {
            old.sites.push(Site {
                id: real(n),
                uplink: Some(by),
                changes: Changes::Server,
                ..Site::new("x", Ipv4Addr::new(10, 0, 0, last))
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
