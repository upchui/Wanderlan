//! Canonical TOML rendering of the configuration.
//!
//! The daemon writes confirmed configurations in this form, and the config
//! fingerprint used by the sync is computed over it, so that two sites with
//! the same configuration always produce byte-identical files.

use std::fmt::Write as _;
use std::time::Duration;

use sha2::{Digest, Sha256};

use super::model::{Local, LocalVid, Network};

const NETWORK_HEADER: &str = "\
# wanderlan network configuration, shared by all sites.
# This file is kept identical on all sites by the wanderlan daemon.
# Change it with the `wanderlan` TUI or edit it and run `wanderlan apply`.
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

/// Renders `network.toml` in canonical form.
pub fn render_network(network: &Network) -> String {
    render_network_inner(network, true)
}

fn render_network_inner(network: &Network, with_revision: bool) -> String {
    let mut out = String::from(NETWORK_HEADER);
    out.push_str("\n[network]\n");
    if with_revision {
        let _ = writeln!(out, "revision = {}", network.revision);
    }
    let _ = writeln!(out, "vlans = {}", quote(&network.vlans.to_string()));
    let _ = writeln!(out, "control_port = {}", network.control_port);
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
    out
}

/// Renders `local.toml` in canonical form.
pub fn render_local(local: &Local) -> String {
    let mut out = String::from(LOCAL_HEADER);
    out.push('\n');
    if let Some(name) = &local.self_name {
        let _ = writeln!(out, "self = {}", quote(name));
    }
    if let Some(trunk) = &local.trunk {
        let _ = writeln!(out, "trunk = {}", quote(trunk));
    }
    for (site, trunk) in &local.tunnel_trunks {
        // Site names are valid bare keys; anything else is quoted.
        let key = if site
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            site.clone()
        } else {
            quote(site)
        };
        let _ = write!(
            out,
            "\n# The tunnel to \"{site}\" has its own trunk (a segment of its own).\n[tunnels.{key}]\ntrunk = {}\n",
            quote(trunk)
        );
    }
    out
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
