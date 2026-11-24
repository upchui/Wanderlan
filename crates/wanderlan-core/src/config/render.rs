//! Canonical TOML rendering of the configuration.
//!
//! The daemon writes confirmed configurations in this form, and the config
//! fingerprint used by the sync is computed over it, so that two sites with
//! the same configuration always produce byte-identical files.

use std::fmt::Write as _;
use std::time::Duration;

use sha2::{Digest, Sha256};

use super::model::{Local, LocalVid, Mesh};

const MESH_HEADER: &str = "\
# wanderlan mesh configuration, shared by all sites.
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

/// Renders `mesh.toml` in canonical form.
pub fn render_mesh(mesh: &Mesh) -> String {
    render_mesh_inner(mesh, true)
}

fn render_mesh_inner(mesh: &Mesh, with_revision: bool) -> String {
    let mut out = String::from(MESH_HEADER);
    out.push_str("\n[mesh]\n");
    if with_revision {
        let _ = writeln!(out, "revision = {}", mesh.revision);
    }
    let _ = writeln!(out, "vlans = {}", quote(&mesh.vlans.to_string()));
    let _ = writeln!(out, "control_port = {}", mesh.control_port);
    let _ = writeln!(
        out,
        "mtu_probe_interval = {}",
        duration(mesh.mtu_probe_interval)
    );
    let _ = writeln!(out, "commit_timeout = {}", duration(mesh.commit_timeout));
    if let Some(mss) = mesh.mss {
        let _ = writeln!(out, "mss = {mss}");
    }
    for site in &mesh.sites {
        out.push_str("\n[[site]]\n");
        let _ = writeln!(out, "name = {}", quote(&site.name));
        let _ = writeln!(out, "address = {}", quote(&site.address.to_string()));
        if let Some(vlans) = &site.vlans {
            let _ = writeln!(out, "vlans = {}", quote(&vlans.to_string()));
        }
    }
    for map in &mesh.maps {
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
    out
}

/// SHA-256 over the canonical mesh form without the revision, hex encoded.
///
/// Two meshes with the same content have the same fingerprint regardless of
/// their revision counter and of formatting or comments in the source file.
pub fn fingerprint(mesh: &Mesh) -> String {
    let digest = Sha256::digest(render_mesh_inner(mesh, false).as_bytes());
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
