//! Semantic validation of a parsed configuration.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use super::diag::{ConfigFile, Diagnostic, FieldPath};
use super::model::{Local, LocalVid, MAX_SITE_NAME_LEN, Mesh, Role};
use crate::topology::{self, GRETAP_PREFIX};

/// Smallest accepted fixed MSS.
pub const MSS_MIN: u16 = 536;
/// Largest accepted fixed MSS (1500 byte inner MTU minus 40).
pub const MSS_MAX: u16 = 1460;
/// Smallest accepted MTU probe interval.
pub const PROBE_INTERVAL_MIN: Duration = Duration::from_secs(10);
/// Accepted range for the commit timeout.
pub const COMMIT_TIMEOUT_RANGE: (Duration, Duration) =
    (Duration::from_secs(10), Duration::from_secs(3600));
/// Maximum interface name length (IFNAMSIZ - 1).
pub const IFNAME_MAX: usize = 15;

/// Validates the mesh and, if given and configured, the local configuration.
///
/// The returned diagnostics have no line numbers; [`super::load`] fills them
/// in from the source spans. The TUI uses the [`FieldPath`]s directly.
pub fn validate(mesh: &Mesh, local: Option<&Local>) -> Vec<Diagnostic> {
    let mut v = Validator::default();
    v.mesh_settings(mesh);
    v.sites(mesh);
    v.maps(mesh);
    v.diags.extend(topology::check(mesh));
    if let Some(local) = local.filter(|l| l.is_configured()) {
        v.local(mesh, local);
    }
    v.diags
}

/// Checks whether a string is a valid site name.
pub fn site_name_error(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("site name is empty".into());
    }
    if name.len() > MAX_SITE_NAME_LEN {
        return Some(format!(
            "site name \"{name}\" is longer than {MAX_SITE_NAME_LEN} characters (it becomes part of the interface name {GRETAP_PREFIX}{name})"
        ));
    }
    let valid_chars = name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    let valid_start = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    if !valid_chars || !valid_start {
        return Some(format!(
            "site name \"{name}\" may only contain a-z, 0-9 and '-' and must not start with '-'"
        ));
    }
    None
}

/// Checks whether a string is a usable network interface name.
pub fn ifname_error(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("interface name is empty".into());
    }
    if name.len() > IFNAME_MAX {
        return Some(format!(
            "interface name \"{name}\" is longer than {IFNAME_MAX} characters"
        ));
    }
    if name == "."
        || name == ".."
        || name
            .chars()
            .any(|c| c == '/' || c == ':' || c.is_whitespace())
    {
        return Some(format!("\"{name}\" is not a valid interface name"));
    }
    None
}

#[derive(Default)]
struct Validator {
    diags: Vec<Diagnostic>,
}

impl Validator {
    fn error(&mut self, path: FieldPath, message: impl Into<String>) {
        self.diags
            .push(Diagnostic::error(ConfigFile::Mesh, path, message));
    }

    fn warning(&mut self, path: FieldPath, message: impl Into<String>) {
        self.diags
            .push(Diagnostic::warning(ConfigFile::Mesh, path, message));
    }

    fn mesh_settings(&mut self, mesh: &Mesh) {
        if mesh.vlans.is_empty() {
            self.error(
                FieldPath::mesh("vlans"),
                "mesh.vlans must contain at least one VLAN",
            );
        }
        if mesh.vlans.contains(1) {
            self.warning(
                FieldPath::mesh("vlans"),
                "mesh.vlans includes VLAN 1, which is the default VLAN of most switches",
            );
        }
        if mesh.mtu_probe_interval < PROBE_INTERVAL_MIN {
            self.error(
                FieldPath::mesh("mtu_probe_interval"),
                format!(
                    "mesh.mtu_probe_interval must be at least {}",
                    humantime::format_duration(PROBE_INTERVAL_MIN)
                ),
            );
        }
        let (min, max) = COMMIT_TIMEOUT_RANGE;
        if mesh.commit_timeout < min || mesh.commit_timeout > max {
            self.error(
                FieldPath::mesh("commit_timeout"),
                format!(
                    "mesh.commit_timeout must be between {} and {}",
                    humantime::format_duration(min),
                    humantime::format_duration(max)
                ),
            );
        }
        if let Some(mss) = mesh.mss.filter(|m| !(MSS_MIN..=MSS_MAX).contains(m)) {
            self.error(
                FieldPath::mesh("mss"),
                format!("mesh.mss must be between {MSS_MIN} and {MSS_MAX}, got {mss}"),
            );
        }
    }

    fn sites(&mut self, mesh: &Mesh) {
        if mesh.sites.is_empty() {
            self.error(FieldPath::root(), "mesh.toml defines no [[site]]");
            return;
        }
        let mut names: BTreeMap<&str, usize> = BTreeMap::new();
        let mut addresses: BTreeMap<std::net::Ipv4Addr, &str> = BTreeMap::new();
        for (i, site) in mesh.sites.iter().enumerate() {
            let name = site.name.as_str();
            if let Some(msg) = site_name_error(name) {
                self.error(FieldPath::site_field(i, "name"), msg);
            }
            if let Some(first) = names.insert(name, i) {
                self.error(
                    FieldPath::site_field(i, "name"),
                    format!(
                        "site \"{name}\" is defined twice (also site #{})",
                        first + 1
                    ),
                );
            }

            let addr = site.address;
            if addr.is_unspecified()
                || addr.is_broadcast()
                || addr.is_multicast()
                || addr.is_loopback()
            {
                self.error(
                    FieldPath::site_field(i, "address"),
                    format!("site \"{name}\": {addr} is not a usable unicast address"),
                );
            }
            if let Some(other) = addresses.insert(addr, name) {
                self.error(
                    FieldPath::site_field(i, "address"),
                    format!("site \"{name}\": address {addr} is already used by site \"{other}\""),
                );
            }

            match site.role {
                Role::Hub => {
                    if site.hub.is_some() {
                        self.error(
                            FieldPath::site_field(i, "hub"),
                            format!("hub \"{name}\" must not have a `hub` entry"),
                        );
                    }
                }
                Role::Spoke => match site.hub.as_deref() {
                    None => self.error(
                        FieldPath::site_field(i, "hub"),
                        format!("spoke \"{name}\" needs `hub = \"<name of its hub>\"`"),
                    ),
                    Some(hub) if hub == name => self.error(
                        FieldPath::site_field(i, "hub"),
                        format!("spoke \"{name}\" cannot be its own hub"),
                    ),
                    Some(hub) => match mesh.site(hub) {
                        None => self.error(
                            FieldPath::site_field(i, "hub"),
                            format!("spoke \"{name}\": hub \"{hub}\" does not exist"),
                        ),
                        Some(h) if h.role != Role::Hub => self.error(
                            FieldPath::site_field(i, "hub"),
                            format!("spoke \"{name}\": \"{hub}\" is a spoke, not a hub"),
                        ),
                        Some(_) => {}
                    },
                },
            }
        }
        if mesh.hubs().next().is_none() {
            self.error(FieldPath::root(), "the mesh needs at least one hub");
        }
    }

    fn maps(&mut self, mesh: &Mesh) {
        // Per site: local side -> map index, shared VID -> map index.
        let mut locals: BTreeMap<(&str, LocalVid), usize> = BTreeMap::new();
        let mut shareds: BTreeMap<(&str, u16), usize> = BTreeMap::new();
        for (i, map) in mesh.maps.iter().enumerate() {
            let site = map.site.as_str();
            if mesh.site(site).is_none() {
                self.error(
                    FieldPath::map_field(i, "site"),
                    format!("map #{}: site \"{site}\" does not exist", i + 1),
                );
            }
            if !mesh.vlans.contains(map.shared) {
                self.error(
                    FieldPath::map_field(i, "shared"),
                    format!(
                        "map #{}: shared VLAN {} is not in mesh.vlans ({})",
                        i + 1,
                        map.shared,
                        mesh.vlans
                    ),
                );
            }
            if map.local == LocalVid::Vid(map.shared) {
                self.error(
                    FieldPath::map_field(i, "local"),
                    format!("map #{}: VLAN {} is mapped onto itself", i + 1, map.shared),
                );
            }
            if let Some(first) = locals.insert((site, map.local), i) {
                let what = match map.local {
                    LocalVid::Native => "has more than one native mapping".to_string(),
                    LocalVid::Vid(v) => format!("maps local VLAN {v} twice"),
                };
                self.error(
                    FieldPath::map_field(i, "local"),
                    format!("site \"{site}\" {what} (map #{} and #{})", first + 1, i + 1),
                );
            }
            if let Some(first) = shareds.insert((site, map.shared), i) {
                self.error(
                    FieldPath::map_field(i, "shared"),
                    format!(
                        "site \"{site}\" maps two local VLANs onto shared VLAN {} (map #{} and #{})",
                        map.shared,
                        first + 1,
                        i + 1
                    ),
                );
            }
            if let LocalVid::Vid(local) = map.local
                && mesh.vlans.contains(local)
            {
                self.warning(
                    FieldPath::map_field(i, "local"),
                    format!(
                        "site \"{site}\": local VLAN {local} is also a shared VLAN; at this site it carries shared VLAN {} and shared VLAN {local} is not available on the trunk",
                        map.shared
                    ),
                );
            }
        }
    }

    fn local(&mut self, mesh: &Mesh, local: &Local) {
        let mut push = |path: &str, msg: String| {
            self.diags.push(Diagnostic::error(
                ConfigFile::Local,
                FieldPath::local(path),
                msg,
            ));
        };
        if let Some(name) = local.self_name.as_deref()
            && mesh.site(name).is_none()
        {
            let known: BTreeSet<&str> = mesh.sites.iter().map(|s| s.name.as_str()).collect();
            let known: Vec<&str> = known.into_iter().collect();
            push(
                "self",
                format!(
                    "self = \"{name}\" is not a site in mesh.toml (known sites: {})",
                    known.join(", ")
                ),
            );
        }
        match local.trunk.as_deref() {
            None => push(
                "trunk",
                "trunk is missing (the local interface carrying the VLANs)".into(),
            ),
            Some(trunk) => {
                if let Some(msg) = ifname_error(trunk) {
                    push("trunk", format!("trunk: {msg}"));
                } else if trunk.starts_with(GRETAP_PREFIX) || trunk == crate::BRIDGE_NAME {
                    push(
                        "trunk",
                        format!("trunk \"{trunk}\" is an interface managed by wanderlan"),
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
