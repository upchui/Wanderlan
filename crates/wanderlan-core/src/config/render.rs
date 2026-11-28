//! Canonical TOML rendering of the configuration.
//!
//! `network.toml` has one form, canonical (the same network always gives the
//! same bytes): every site in a table named by its identity, with its name
//! inside, and every reference an identity (the name in a comment). Names
//! are for people: the daemon writes, sends and fingerprints only this
//! form; files written by hand may name sites by their names (and older
//! forms are read), they are written back in this form.

use std::fmt::Write as _;
use std::time::Duration;

use sha2::{Digest, Sha256};

use super::model::{
    Breakouts, ConntrackSkip, Local, LocalVid, Network, RpsMode, SitePorts, TunnelVlans,
};
use crate::identity::SiteId;

const NETWORK_HEADER: &str = "\
# wanderlan network configuration, shared by all sites.
# This file is kept identical on all sites by the wanderlan daemon.
# Change it with the `wanderlan` TUI or edit it and run `wanderlan apply`.
#
# [site.\"<identity>\"]: a site, `name` is what it is called (several sites
# may have the same name); every other site is referred to by its identity.
# `server` is the site it is a client of (none: a server); changes = who
# changes the tunnel to it (\"server\", \"client\" or \"both\");
# map = { <source VLAN> = <VLAN on its trunk, or \"native\"> };
# tunnel_vlans = { \"<other site>\" = <the most that tunnel carries> };
# breakout = { <VLAN> = { uplink = <interface>, except = [<networks>] } }:
# at a client, the public destinations of the VLAN leave through its own uplink.
# By hand a site may be named by its name instead (`server = \"hq\"`, a new
# site as [site.hq]): the daemon writes it back with the identities.
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

/// The site that holds the limit of a tunnel (in its table): the client
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

/// Renders `network.toml` as the daemon writes it to disk, as the TUI and
/// the CLI show and send it.
pub fn render_network(network: &Network) -> String {
    render_form(network, true)
}

/// Renders `network.toml` as the sites exchange it (the same form).
pub fn render_network_wire(network: &Network) -> String {
    render_form(network, true)
}

/// The form of `network.toml`: the sites by their identity, every
/// reference an identity with the name as a comment.
fn render_form(network: &Network, with_revision: bool) -> String {
    let mut out = String::from(NETWORK_HEADER);
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
        let site = network.site(id);
        // A placeholder until a box joins as the site (it has no key).
        let placeholder = if site.is_some_and(|s| !s.has_identity()) {
            "  # no box has joined as this site yet"
        } else {
            ""
        };
        let _ = writeln!(out, "\n[site.{}]{placeholder}", quote(&id.to_string()));
        if let Some(site) = site {
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
            // Who changes the tunnel to its server: only a client has one.
            if site.uplink.is_some() {
                let _ = writeln!(out, "changes = {}", quote(site.changes.as_str()));
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

/// The rules of a site, after its references.
fn site_rules(out: &mut String, network: &Network, site: &super::model::Site) {
    let _ = network;
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

/// `map = { .. }` of a site.
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
    if local.rps_transport != RpsMode::default() {
        let _ = writeln!(out, "rps_transport = \"{}\"", local.rps_transport.as_str());
    }
    if local.rps_trunk != RpsMode::default() {
        let _ = writeln!(out, "rps_trunk = \"{}\"", local.rps_trunk.as_str());
    }
    if local.skip_conntrack != ConntrackSkip::default() {
        let _ = writeln!(
            out,
            "skip_conntrack = \"{}\"",
            local.skip_conntrack.as_str()
        );
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
/// encoded.
///
/// Two networks with the same content have the same fingerprint regardless of
/// their revision counter, of formatting or comments in the source file and
/// of the order of their mappings and tunnel limits.
pub fn fingerprint(network: &Network) -> String {
    let mut canonical = network.clone();
    canonical.canonicalize();
    let text = render_form(&canonical, false);
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
