//! Canonical TOML rendering of the configuration.
//!
//! The forms of `network.toml`, all canonical (the same network always gives
//! the same bytes):
//! * [`render_network`]: what the daemon writes to disk, what the TUI and
//!   the CLI show and send. Format 3 (sites by identity) once every site has
//!   an identity of its own or two sites share a name, else format 2.
//! * [`render_network_wire`]: what the sites send each other and what the
//!   fingerprint of the sync is computed over. Format 1 as long as the
//!   network can be written with names alone (older sites read only that);
//!   it never changes, so that sites of different versions keep
//!   understanding each other and see the same network as the same. Format
//!   3 otherwise.

use std::fmt::Write as _;
use std::time::Duration;

use sha2::{Digest, Sha256};

use super::model::{Breakouts, Local, LocalVid, Network, SitePorts, TunnelVlans};
use crate::identity::SiteId;

const NETWORK_HEADER: &str = "\
# wanderlan network configuration, shared by all sites.
# This file is kept identical on all sites by the wanderlan daemon.
# Change it with the `wanderlan` TUI or edit it and run `wanderlan apply`.
";

const NETWORK_HEADER_2: &str = "\
# wanderlan network configuration, shared by all sites.
# This file is kept identical on all sites by the wanderlan daemon.
# Change it with the `wanderlan` TUI or edit it and run `wanderlan apply`.
#
# [site.<name>]: `server` is the site it is a client of (none: a server);
# map = { <source VLAN> = <VLAN on its trunk, or \"native\"> };
# tunnel_vlans = { <other site> = <the most that tunnel carries> };
# breakout = { <VLAN> = { uplink = <interface>, except = [<networks>] } }:
# at a client, the public destinations of the VLAN leave through its own uplink.
";

const NETWORK_HEADER_3: &str = "\
# wanderlan network configuration, shared by all sites.
# This file is kept identical on all sites by the wanderlan daemon.
# Change it with the `wanderlan` TUI or edit it and run `wanderlan apply`.
#
# [site.\"<identity>\"]: a site, `name` is what it is called (several sites
# may have the same name); every other site is referred to by its identity.
# `server` is the site it is a client of (none: a server);
# map = { <source VLAN> = <VLAN on its trunk, or \"native\"> };
# tunnel_vlans = { \"<other site>\" = <the most that tunnel carries> };
# breakout = { <VLAN> = { uplink = <interface>, except = [<networks>] } }:
# at a client, the public destinations of the VLAN leave through its own uplink.
";

const LOCAL_HEADER: &str = "\
# wanderlan site-local configuration (never synchronised).
";

fn quote(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

fn duration(d: Duration) -> String {
    quote(&humantime::format_duration(d).to_string())
}

/// True if `network` can be written in format 1 or 2, with names: no two
/// sites share a name, every site without an identity of its own has the
/// one of its name, and every reference names a site of the network or a
/// site it does not have by a name none of its sites has.
pub fn expressible_by_names(network: &Network) -> bool {
    if !network.names_unique() {
        return false;
    }
    if network
        .sites
        .iter()
        .any(|s| !s.has_identity() && s.id != SiteId::provisional(&s.name))
    {
        return false;
    }
    let fine = |id: SiteId| {
        network.site(id).is_some()
            || id
                .provisional_name()
                .is_some_and(|name| network.named(name).next().is_none())
    };
    network
        .sites
        .iter()
        .flat_map(|s| s.uplink.iter().chain(s.managed_by.iter()))
        .chain(network.maps.iter().map(|m| &m.site))
        .chain(network.tunnels.iter().flat_map(|t| t.sites.iter()))
        .all(|id| fine(*id))
}

/// A site as a reference in format 1 or 2: its name; a site the network
/// does not have by the name of its provisional identity, else by its
/// identity.
fn named(network: &Network, id: SiteId) -> String {
    match network.site(id) {
        Some(site) => site.name.clone(),
        None => id
            .provisional_name()
            .map_or_else(|| id.to_string(), str::to_string),
    }
}

/// The site that holds the limit of a tunnel in formats 2 and 3: the client
/// of the tunnel, between two servers the one listed first, never a site
/// that is not in the list.
fn holder(network: &Network, tunnel: &TunnelVlans) -> SiteId {
    let [a, b] = tunnel.sites;
    let position = |id: SiteId| network.sites.iter().position(|s| s.id == id);
    let uplink = |id: SiteId| network.site(id).and_then(|s| s.uplink);
    if uplink(a) == Some(b) {
        a
    } else if uplink(b) == Some(a) {
        b
    } else {
        match (position(a), position(b)) {
            (Some(pa), Some(pb)) if pb < pa => b,
            (None, Some(_)) => b,
            _ => a,
        }
    }
}

/// The format `network.toml` is written in: 3 once every site has an
/// identity of its own (or two share a name), else (also without sites) 2.
pub fn disk_format(network: &Network) -> u8 {
    if !network.sites.is_empty() && (network.all_identified() || !network.names_unique()) {
        3
    } else {
        2
    }
}

/// Renders `network.toml` as it is written to disk (see [`disk_format`]).
pub fn render_network(network: &Network) -> String {
    if disk_format(network) == 3 {
        render_network_3(network, true)
    } else {
        render_network_2(network)
    }
}

/// Renders `network.toml` in format 2.
fn render_network_2(network: &Network) -> String {
    let mut out = String::from(NETWORK_HEADER_2);
    network_section(&mut out, network, true);
    let holders: Vec<SiteId> = network.tunnels.iter().map(|t| holder(network, t)).collect();
    let mut owners: Vec<SiteId> = network.sites.iter().map(|s| s.id).collect();
    // A mapping or limit of a site that is not in the list (a draft with an
    // error) still gets written, in a table of its own.
    for id in network
        .maps
        .iter()
        .map(|m| m.site)
        .chain(holders.iter().copied())
    {
        if !owners.contains(&id) {
            owners.push(id);
        }
    }
    let mut written = std::collections::BTreeSet::new();
    for id in owners {
        let name = named(network, id);
        // Each name once (a draft can list a site twice).
        if !written.insert(name.clone()) {
            continue;
        }
        let _ = writeln!(out, "\n[site.{}]", key(&name));
        if let Some(site) = network.site(id) {
            let _ = writeln!(out, "address = {}", quote(&site.address.to_string()));
            if let Some(id) = site.own_id() {
                let _ = writeln!(out, "id = {}", quote(&id.to_string()));
            }
            if let Some(up) = site.uplink {
                let _ = writeln!(out, "server = {}", quote(&named(network, up)));
            }
            if let Some(by) = site.managed_by {
                let _ = writeln!(out, "managed_by = {}", quote(&named(network, by)));
            }
            site_rules(&mut out, network, site);
        }
        site_maps(&mut out, network, id);
        let limits: Vec<String> = network
            .tunnels
            .iter()
            .zip(&holders)
            .filter(|(_, h)| **h == id)
            .map(|(t, _)| {
                format!(
                    "{} = {}",
                    key(&named(network, t.other(id))),
                    quote(&limit_text(t))
                )
            })
            .collect();
        if !limits.is_empty() {
            let _ = writeln!(out, "tunnel_vlans = {{ {} }}", limits.join(", "));
        }
    }
    out
}

/// Renders `network.toml` in format 3: the sites by their identity, every
/// reference an identity (with the name as a comment where `comments`).
fn render_network_3(network: &Network, with_revision: bool) -> String {
    let mut out = String::from(NETWORK_HEADER_3);
    network_section(&mut out, network, with_revision);
    let holders: Vec<SiteId> = network.tunnels.iter().map(|t| holder(network, t)).collect();
    let mut owners: Vec<SiteId> = network.sites.iter().map(|s| s.id).collect();
    for id in network
        .maps
        .iter()
        .map(|m| m.site)
        .chain(holders.iter().copied())
    {
        if !owners.contains(&id) {
            owners.push(id);
        }
    }
    let mut written = std::collections::BTreeSet::new();
    for id in owners {
        if !written.insert(id) {
            continue;
        }
        let _ = writeln!(out, "\n[site.{}]", quote(&id.to_string()));
        if let Some(site) = network.site(id) {
            let _ = writeln!(out, "name = {}", quote(&site.name));
            let _ = writeln!(out, "address = {}", quote(&site.address.to_string()));
            if let Some(up) = site.uplink {
                let _ = writeln!(
                    out,
                    "server = {}  # {}",
                    quote(&up.to_string()),
                    comment(&network.label(up))
                );
            }
            if let Some(by) = site.managed_by {
                let _ = writeln!(
                    out,
                    "managed_by = {}  # {}",
                    quote(&by.to_string()),
                    comment(&network.label(by))
                );
            }
            site_rules(&mut out, network, site);
        }
        site_maps(&mut out, network, id);
        let limits: Vec<(String, String)> = network
            .tunnels
            .iter()
            .zip(&holders)
            .filter(|(_, h)| **h == id)
            .map(|(t, _)| {
                let other = t.other(id);
                (
                    format!("{} = {}", quote(&other.to_string()), quote(&limit_text(t))),
                    network.label(other),
                )
            })
            .collect();
        if !limits.is_empty() {
            let (entries, names): (Vec<String>, Vec<String>) = limits.into_iter().unzip();
            let _ = writeln!(
                out,
                "tunnel_vlans = {{ {} }}  # {}",
                entries.join(", "),
                comment(&names.join(", "))
            );
        }
    }
    out
}

/// A name in a comment: one line, whatever it holds.
fn comment(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// `[network]` with its settings.
fn network_section(out: &mut String, network: &Network, with_revision: bool) {
    out.push_str("\n[network]\n");
    if with_revision {
        let _ = writeln!(out, "revision = {}", network.revision);
    }
    let _ = writeln!(out, "vlans = {}", quote(&network.vlans.to_string()));
    let _ = writeln!(out, "control_port = {}", network.control_port);
    if let Some(port) = network.tunnel_port {
        let _ = writeln!(out, "tunnel_port = {port}");
    }
    let _ = writeln!(
        out,
        "mtu_probe_interval = {}",
        duration(network.mtu_probe_interval)
    );
    let _ = writeln!(out, "commit_timeout = {}", duration(network.commit_timeout));
    if let Some(mss) = network.mss {
        let _ = writeln!(out, "mss = {mss}");
    }
}

/// The rules of a site in formats 2 and 3, after its references.
fn site_rules(out: &mut String, network: &Network, site: &super::model::Site) {
    let _ = network;
    if site.self_managed {
        out.push_str("self_managed = true\n");
    }
    if !site.online {
        out.push_str("online = false\n");
    }
    if let Some(vlans) = &site.vlans {
        let _ = writeln!(out, "vlans = {}", quote(&vlans.to_string()));
    }
    if let Some(own) = site.own_vlans.as_ref().filter(|v| !v.is_empty()) {
        let _ = writeln!(out, "own_vlans = {}", quote(&own.to_string()));
    }
    if let Some(line) = breakout_line(&site.breakout) {
        out.push_str(&line);
    }
    out.push_str(&ports_lines(site.ports));
}

/// `map = { .. }` of a site in formats 2 and 3.
fn site_maps(out: &mut String, network: &Network, id: SiteId) {
    let maps: Vec<String> = network
        .maps_of(id)
        .map(|m| match m.local {
            LocalVid::Native => format!("{} = \"native\"", m.shared),
            LocalVid::Vid(v) => format!("{} = {v}", m.shared),
        })
        .collect();
    if !maps.is_empty() {
        let _ = writeln!(out, "map = {{ {} }}", maps.join(", "));
    }
}

/// The VLANs of a tunnel limit as written.
fn limit_text(tunnel: &TunnelVlans) -> String {
    if tunnel.vlans.is_empty() {
        "none".to_string()
    } else {
        tunnel.vlans.to_string()
    }
}

/// Renders `network.toml` as the sites exchange it: format 1, kept byte for
/// byte as wanderlan 1.0 wrote it, as long as the network can be written
/// with names ([`expressible_by_names`]): older sites read only that, and
/// [`fingerprint`] hashes it. Format 3 otherwise.
pub fn render_network_wire(network: &Network) -> String {
    if expressible_by_names(network) {
        render_network_1(network, true)
    } else {
        render_network_3(network, true)
    }
}

/// A site name as a TOML key: bare where it can be, otherwise quoted.
fn key(name: &str) -> String {
    if !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        name.to_string()
    } else {
        quote(name)
    }
}

/// Format 1: `[[site]]`, `[[map]]` and `[[tunnel]]`, references by name.
fn render_network_1(network: &Network, with_revision: bool) -> String {
    let mut out = String::from(NETWORK_HEADER);
    network_section(&mut out, network, with_revision);
    for site in &network.sites {
        out.push_str("\n[[site]]\n");
        let _ = writeln!(out, "name = {}", quote(&site.name));
        let _ = writeln!(out, "address = {}", quote(&site.address.to_string()));
        if let Some(id) = site.own_id() {
            let _ = writeln!(out, "id = {}", quote(&id.to_string()));
        }
        if let Some(vlans) = &site.vlans {
            let _ = writeln!(out, "vlans = {}", quote(&vlans.to_string()));
        }
        if let Some(own) = site.own_vlans.as_ref().filter(|v| !v.is_empty()) {
            let _ = writeln!(out, "own_vlans = {}", quote(&own.to_string()));
        }
        if !site.online {
            out.push_str("online = false\n");
        }
        if let Some(by) = site.managed_by {
            let _ = writeln!(out, "managed_by = {}", quote(&named(network, by)));
        }
        if site.self_managed {
            out.push_str("self_managed = true\n");
        }
        // Only where it is set: networks without a breakout keep the form
        // (and the fingerprint) older sites know.
        if let Some(line) = breakout_line(&site.breakout) {
            out.push_str(&line);
        }
        // Only where set: older sites know the form without them.
        out.push_str(&ports_lines(site.ports));
        match site.uplink {
            Some(up) => {
                let _ = writeln!(out, "uplink = {}", quote(&named(network, up)));
            }
            // A server with a manager says so, otherwise it would be read
            // as a client of its manager (configurations before uplinks).
            None if site.managed_by.is_some() => out.push_str("uplink = \"\"\n"),
            None => {}
        }
    }
    for map in &network.maps {
        out.push_str("\n[[map]]\n");
        let _ = writeln!(out, "site = {}", quote(&named(network, map.site)));
        match map.local {
            LocalVid::Native => out.push_str("local = \"native\"\n"),
            LocalVid::Vid(v) => {
                let _ = writeln!(out, "local = {v}");
            }
        }
        let _ = writeln!(out, "shared = {}", map.shared);
    }
    for tunnel in &network.tunnels {
        out.push_str("\n[[tunnel]]\n");
        // The smaller name first, as 1.0 wrote it.
        let [a, b] = tunnel.sites.map(|id| named(network, id));
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        let _ = writeln!(out, "between = [{}, {}]", quote(&a), quote(&b));
        let _ = writeln!(out, "vlans = {}", quote(&limit_text(tunnel)));
    }
    out
}

/// `breakout = { 10 = {}, 20 = { uplink = "ens20", except = [..] } }`, or
/// nothing without a breakout.
fn breakout_line(breakouts: &Breakouts) -> Option<String> {
    if breakouts.is_empty() {
        return None;
    }
    let entries: Vec<String> = breakouts
        .iter()
        .map(|(vid, b)| {
            let mut fields = Vec::new();
            if let Some(uplink) = &b.uplink {
                fields.push(format!("uplink = {}", quote(uplink)));
            }
            if !b.except.is_empty() {
                let nets: Vec<String> = b.except.iter().map(|n| quote(&n.to_string())).collect();
                fields.push(format!("except = [{}]", nets.join(", ")));
            }
            if fields.is_empty() {
                format!("{vid} = {{}}")
            } else {
                format!("{vid} = {{ {} }}", fields.join(", "))
            }
        })
        .collect();
    Some(format!("breakout = {{ {} }}\n", entries.join(", ")))
}

/// Renders `local.toml` in canonical form; the names of the sites (from
/// `network`) go into comments.
pub fn render_local(local: &Local, network: Option<&Network>) -> String {
    let mut out = String::from(LOCAL_HEADER);
    out.push('\n');
    let name = |id: SiteId| {
        network.map_or_else(
            || {
                id.provisional_name()
                    .map_or_else(|| id.fingerprint(), str::to_string)
            },
            |n| n.label(id),
        )
    };
    if let Some(id) = local.self_id {
        let _ = writeln!(
            out,
            "self = {}  # {}",
            quote(&id.to_string()),
            comment(&name(id))
        );
    }
    if !local.trunk.is_empty() {
        let _ = writeln!(out, "trunk = {}", ports(&local.trunk));
    }
    if let Some(t) = local.commit_timeout {
        let _ = writeln!(out, "commit_timeout = {}", duration(t));
    }
    if let Some(t) = local.mtu_probe_interval {
        let _ = writeln!(out, "mtu_probe_interval = {}", duration(t));
    }
    if let Some(mss) = local.mss {
        let _ = writeln!(out, "mss = {mss}");
    }
    for (site, trunk) in &local.tunnel_trunks {
        let _ = write!(
            out,
            "\n# The tunnel to \"{}\" has its own trunk (a segment of its own).\n[tunnels.{}]\ntrunk = {}\n",
            comment(&name(*site)),
            quote(&site.to_string()),
            ports(trunk)
        );
    }
    out
}

/// The own ports of a site (nothing for the default ones).
fn ports_lines(ports: SitePorts) -> String {
    let mut out = String::new();
    if let Some(port) = ports.control {
        let _ = writeln!(out, "control_port = {port}");
    }
    if let Some(port) = ports.tunnel {
        let _ = writeln!(out, "tunnel_port = {port}");
    }
    out
}

/// One port as a string, several as a list.
fn ports(ports: &[String]) -> String {
    match ports {
        [one] => quote(one),
        many => format!(
            "[{}]",
            many.iter().map(|p| quote(p)).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// SHA-256 over the canonical network form without the revision, hex
/// encoded: format 1 where the sites exchange that, else format 3.
///
/// Two networks with the same content have the same fingerprint regardless of
/// their revision counter, of formatting or comments in the source file and
/// of the order of their mappings and tunnel limits.
pub fn fingerprint(network: &Network) -> String {
    let mut canonical = network.clone();
    canonical.canonicalize();
    let text = if expressible_by_names(&canonical) {
        render_network_1(&canonical, false)
    } else {
        render_network_3(&canonical, false)
    };
    hex(&Sha256::digest(text.as_bytes()))
}

/// SHA-256 over the canonical `local.toml`, hex encoded (without the names
/// of the sites, which are only comments).
pub fn local_fingerprint(local: &Local) -> String {
    hex(&Sha256::digest(render_local(local, None).as_bytes()))
}

fn hex(digest: &[u8]) -> String {
    let mut hex = String::with_capacity(64);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Unified diff between two texts (empty if they are equal).
pub fn unified_diff(old_label: &str, old: &str, new_label: &str, new: &str) -> String {
    if old == new {
        return String::new();
    }
    similar::TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(old_label, new_label)
        .to_string()
}
