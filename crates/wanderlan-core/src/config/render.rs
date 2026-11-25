//! Canonical TOML rendering of the configuration.
//!
//! Two forms of `network.toml`, both canonical (the same network always
//! gives the same bytes):
//! * [`render_network`], format 2: what the daemon writes to disk, what the
//!   TUI and the CLI show and send.
//! * [`render_network_wire`], format 1: what the sites send each other and
//!   what the fingerprint of the sync is computed over. It never changes, so
//!   that sites of different versions keep understanding each other and see
//!   the same network as the same.

use std::fmt::Write as _;
use std::time::Duration;

use sha2::{Digest, Sha256};

use super::model::{Local, LocalVid, Network, TunnelMode};

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
# tunnel_vlans = { <other site> = <the most that tunnel carries> }.
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

/// Renders `network.toml` in format 2, the form of the file on disk.
pub fn render_network(network: &Network) -> String {
    let mut out = String::from(NETWORK_HEADER_2);
    out.push_str("\n[network]\n");
    let _ = writeln!(out, "revision = {}", network.revision);
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
    // Every limit goes to one of its two sites: the client of the tunnel,
    // between two servers the one listed first, never a site that is not
    // in the list.
    let position = |name: &str| network.sites.iter().position(|s| s.name == name);
    let holder = |[a, b]: &[String; 2]| -> String {
        let uplink = |name: &str| network.site(name).and_then(|s| s.uplink.as_deref());
        let first = if uplink(a) == Some(b.as_str()) {
            a
        } else if uplink(b) == Some(a.as_str()) {
            b
        } else {
            match (position(a), position(b)) {
                (Some(pa), Some(pb)) if pb < pa => b,
                (None, Some(_)) => b,
                _ => a,
            }
        };
        first.clone()
    };
    let holders: Vec<String> = network.tunnels.iter().map(|t| holder(&t.sites)).collect();
    let mut names: Vec<&str> = network.sites.iter().map(|s| s.name.as_str()).collect();
    // A mapping or limit of a site that is not in the list (a draft with an
    // error) still gets written, in a table of its own.
    for name in network
        .maps
        .iter()
        .map(|m| m.site.as_str())
        .chain(holders.iter().map(String::as_str))
    {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    let mut written = std::collections::BTreeSet::new();
    for name in names {
        // Each name once (a draft can list a site twice).
        if !written.insert(name) {
            continue;
        }
        let _ = writeln!(out, "\n[site.{}]", key(name));
        if let Some(site) = network.site(name) {
            let _ = writeln!(out, "address = {}", quote(&site.address.to_string()));
            if let Some(id) = &site.id {
                let _ = writeln!(out, "id = {}", quote(&id.to_string()));
            }
            if let Some(up) = &site.uplink {
                let _ = writeln!(out, "server = {}", quote(up));
            }
            if let Some(by) = &site.managed_by {
                let _ = writeln!(out, "managed_by = {}", quote(by));
            }
            if site.self_managed {
                out.push_str("self_managed = true\n");
            }
            if !site.online {
                out.push_str("online = false\n");
            }
            if site.tunnel_mode != TunnelMode::default() {
                let _ = writeln!(out, "tunnel_mode = {}", quote(site.tunnel_mode.as_str()));
            }
            if let Some(vlans) = &site.vlans {
                let _ = writeln!(out, "vlans = {}", quote(&vlans.to_string()));
            }
            if let Some(own) = site.own_vlans.as_ref().filter(|v| !v.is_empty()) {
                let _ = writeln!(out, "own_vlans = {}", quote(&own.to_string()));
            }
        }
        let maps: Vec<String> = network
            .maps
            .iter()
            .filter(|m| m.site == name)
            .map(|m| match m.local {
                LocalVid::Native => format!("{} = \"native\"", m.shared),
                LocalVid::Vid(v) => format!("{} = {v}", m.shared),
            })
            .collect();
        if !maps.is_empty() {
            let _ = writeln!(out, "map = {{ {} }}", maps.join(", "));
        }
        let limits: Vec<String> = network
            .tunnels
            .iter()
            .zip(&holders)
            .filter(|(_, h)| h.as_str() == name)
            .map(|(t, _)| {
                let other = if t.sites[0] == name {
                    &t.sites[1]
                } else {
                    &t.sites[0]
                };
                let vlans = if t.vlans.is_empty() {
                    "none".to_string()
                } else {
                    t.vlans.to_string()
                };
                format!("{} = {}", key(other), quote(&vlans))
            })
            .collect();
        if !limits.is_empty() {
            let _ = writeln!(out, "tunnel_vlans = {{ {} }}", limits.join(", "));
        }
    }
    out
}

/// Renders `network.toml` in format 1, the form the sites exchange. Kept
/// byte for byte as wanderlan 1.0 wrote it: older sites read only this, and
/// [`fingerprint`] hashes it.
pub fn render_network_wire(network: &Network) -> String {
    render_network_inner(network, true)
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

fn render_network_inner(network: &Network, with_revision: bool) -> String {
    let mut out = String::from(NETWORK_HEADER);
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
    for site in &network.sites {
        out.push_str("\n[[site]]\n");
        let _ = writeln!(out, "name = {}", quote(&site.name));
        let _ = writeln!(out, "address = {}", quote(&site.address.to_string()));
        if let Some(id) = &site.id {
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
        if let Some(by) = &site.managed_by {
            let _ = writeln!(out, "managed_by = {}", quote(by));
        }
        if site.self_managed {
            out.push_str("self_managed = true\n");
        }
        if site.tunnel_mode != TunnelMode::default() {
            let _ = writeln!(out, "tunnel_mode = {}", quote(site.tunnel_mode.as_str()));
        }
        match &site.uplink {
            Some(up) => {
                let _ = writeln!(out, "uplink = {}", quote(up));
            }
            // A server with a manager says so, otherwise it would be read
            // as a client of its manager (configurations before uplinks).
            None if site.managed_by.is_some() => out.push_str("uplink = \"\"\n"),
            None => {}
        }
    }
    for map in &network.maps {
        out.push_str("\n[[map]]\n");
        let _ = writeln!(out, "site = {}", quote(&map.site));
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
        let [a, b] = &tunnel.sites;
        let _ = writeln!(out, "between = [{}, {}]", quote(a), quote(b));
        let vlans = if tunnel.vlans.is_empty() {
            "none".to_string()
        } else {
            tunnel.vlans.to_string()
        };
        let _ = writeln!(out, "vlans = {}", quote(&vlans));
    }
    out
}

/// Renders `local.toml` in canonical form.
pub fn render_local(local: &Local) -> String {
    let mut out = String::from(LOCAL_HEADER);
    out.push('\n');
    if let Some(name) = &local.self_name {
        let _ = writeln!(out, "self = {}", quote(name));
    }
    if !local.trunk.is_empty() {
        let _ = writeln!(out, "trunk = {}", ports(&local.trunk));
    }
    for (site, trunk) in &local.tunnel_trunks {
        let key = key(site);
        // The comment is one line, whatever the name holds.
        let shown: String = site
            .chars()
            .map(|c| if c.is_control() { '?' } else { c })
            .collect();
        let _ = write!(
            out,
            "\n# The tunnel to \"{shown}\" has its own trunk (a segment of its own).\n[tunnels.{key}]\ntrunk = {}\n",
            ports(trunk)
        );
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

/// SHA-256 over the canonical network form without the revision, hex encoded.
///
/// Two networks with the same content have the same fingerprint regardless of
/// their revision counter and of formatting or comments in the source file.
pub fn fingerprint(network: &Network) -> String {
    let digest = Sha256::digest(render_network_inner(network, false).as_bytes());
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
