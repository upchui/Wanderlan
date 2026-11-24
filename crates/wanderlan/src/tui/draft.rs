//! The running configuration and the draft the user edits.
//!
//! All edits go into a [`Draft`], a copy of the running configuration. The
//! draft is validated after every change with the same rules the daemon
//! uses; the diagnostics carry [`FieldPath`]s so the views can mark the
//! offending rows and fields.

use wanderlan_core::api::ConfigTexts;
use wanderlan_core::config::{
    Diagnostic, FieldPath, Local, Network, Severity, Site, VlanSet, parse_local, parse_network,
    render_local, render_network, site_pair, validate,
};
use wanderlan_core::topology;

use super::diff::{self, DiffLine};

/// The configuration the daemon runs, as returned by `get_config`.
#[derive(Debug, Clone, PartialEq)]
pub struct Running {
    /// True if the site has a configuration.
    pub configured: bool,
    /// Network revision.
    pub revision: u64,
    /// Network fingerprint.
    pub fingerprint: String,
    /// The network, if there is one.
    pub network: Option<Network>,
    /// The local settings.
    pub local: Local,
}

impl Running {
    /// Parses the texts sent by the daemon.
    pub fn parse(texts: &ConfigTexts) -> Result<Self, String> {
        let first = |file: &str, diags: Vec<Diagnostic>| {
            let detail = diags
                .first()
                .map_or_else(|| "unknown error".to_string(), ToString::to_string);
            format!("cannot read the running {file}: {detail}")
        };
        let network = if texts.network.trim().is_empty() {
            None
        } else {
            parse_network(&texts.network)
                .map_err(|d| first("network.toml", d))?
                .network
        };
        let local = parse_local(&texts.local)
            .map_err(|d| first("local.toml", d))?
            .local;
        Ok(Self {
            configured: texts.configured,
            revision: texts.revision,
            fingerprint: texts.fingerprint.clone(),
            network,
            local,
        })
    }

    /// Canonical `network.toml` (empty if there is no network).
    pub fn network_text(&self) -> String {
        self.network
            .as_ref()
            .map(render_network)
            .unwrap_or_default()
    }

    /// Canonical `local.toml`.
    pub fn local_text(&self) -> String {
        render_local(&self.local)
    }
}

/// The configuration being edited.
#[derive(Debug, Clone, PartialEq)]
pub struct Draft {
    /// Revision of the running configuration the draft was copied from.
    pub base_revision: u64,
    /// The edited network.
    pub network: Network,
    /// The edited local settings.
    pub local: Local,
    /// Result of validating `network` and `local`.
    pub diagnostics: Vec<Diagnostic>,
}

impl Draft {
    /// Creates a draft and validates it.
    pub fn new(base_revision: u64, network: Network, local: Local) -> Self {
        let mut draft = Self {
            base_revision,
            network,
            local,
            diagnostics: Vec::new(),
        };
        draft.revalidate();
        draft
    }

    /// A draft of the running configuration, if it has a network.
    pub fn from_running(running: &Running) -> Option<Self> {
        running
            .network
            .clone()
            .map(|network| Self::new(running.revision, network, running.local.clone()))
    }

    /// Validates the draft again after a change.
    pub fn revalidate(&mut self) {
        self.diagnostics = validate(&self.network, Some(&self.local));
    }

    /// True if the draft differs from the running configuration (the
    /// revision counter is ignored).
    pub fn differs_from(&self, running: &Running) -> bool {
        match &running.network {
            None => true,
            Some(network) => !same_network(&self.network, network) || self.local != running.local,
        }
    }

    /// Canonical `network.toml` of the draft.
    pub fn network_text(&self) -> String {
        render_network(&self.network)
    }

    /// Canonical `local.toml` of the draft.
    pub fn local_text(&self) -> String {
        render_local(&self.local)
    }

    /// Number of errors.
    pub fn errors(&self) -> usize {
        self.diagnostics.iter().filter(|d| d.is_error()).count()
    }

    /// Diagnostics exactly at `path`.
    pub fn at<'a>(&'a self, path: &'a FieldPath) -> impl Iterator<Item = &'a Diagnostic> + 'a {
        self.diagnostics.iter().filter(move |d| d.path == *path)
    }

    /// Diagnostics at `path` or below it (`site[1]` covers `site[1].hub`).
    pub fn within<'a>(&'a self, path: &'a FieldPath) -> impl Iterator<Item = &'a Diagnostic> + 'a {
        self.diagnostics
            .iter()
            .filter(move |d| is_within(&d.path, path))
    }

    /// The worst severity at or below `path`.
    pub fn worst_within(&self, path: &FieldPath) -> Option<Severity> {
        worst(self.within(path))
    }

    /// The worst severity exactly at `path`.
    pub fn worst_at(&self, path: &FieldPath) -> Option<Severity> {
        worst(self.at(path))
    }

    /// Removes a site together with its VLAN mappings.
    pub fn remove_site(&mut self, index: usize) -> Option<Site> {
        if index >= self.network.sites.len() {
            return None;
        }
        let site = self.network.sites.remove(index);
        self.network.maps.retain(|m| m.site != site.name);
        self.network.tunnels.retain(|t| !t.touches(&site.name));
        self.revalidate();
        Some(site)
    }

    /// Removes a VLAN mapping.
    pub fn remove_map(&mut self, index: usize) -> bool {
        if index >= self.network.maps.len() {
            return false;
        }
        self.network.maps.remove(index);
        self.revalidate();
        true
    }

    /// Updates all references to a renamed site (mappings, `self`).
    pub fn rename_references(&mut self, old: &str, new: &str) {
        for map in &mut self.network.maps {
            if map.site == old {
                map.site = new.to_string();
            }
        }
        for site in &mut self.network.sites {
            if site.managed_by.as_deref() == Some(old) {
                site.managed_by = Some(new.to_string());
            }
            if site.uplink.as_deref() == Some(old) {
                site.uplink = Some(new.to_string());
            }
        }
        for tunnel in &mut self.network.tunnels {
            if tunnel.touches(old) {
                let [a, b] = &tunnel.sites;
                let rename = |n: &String| if n == old { new.to_string() } else { n.clone() };
                tunnel.sites = site_pair(&rename(a), &rename(b));
            }
        }
        if self.local.self_name.as_deref() == Some(old) {
            self.local.self_name = Some(new.to_string());
        }
        if let Some(trunk) = self.local.tunnel_trunks.remove(old) {
            self.local.tunnel_trunks.insert(new.to_string(), trunk);
        }
    }

    /// How many things the draft changes against `running`: settings,
    /// sites, mappings, tunnel limits and local settings (an edited mapping
    /// or limit counts once).
    pub fn changes(&self, running: Option<&Running>) -> usize {
        let Some(old) = running.and_then(|r| r.network.as_ref()) else {
            return 0;
        };
        let new = &self.network;
        let settings = [
            old.vlans != new.vlans,
            old.control_port != new.control_port,
            old.mtu_probe_interval != new.mtu_probe_interval,
            old.mss != new.mss,
            old.commit_timeout != new.commit_timeout,
        ]
        .iter()
        .filter(|c| **c)
        .count();
        let sites = new
            .sites
            .iter()
            .filter(|s| old.site(&s.name) != Some(*s))
            .count()
            + old
                .sites
                .iter()
                .filter(|s| new.site(&s.name).is_none())
                .count();
        let added = new.maps.iter().filter(|m| !old.maps.contains(m)).count();
        let removed = old.maps.iter().filter(|m| !new.maps.contains(m)).count();
        let maps = added.max(removed);
        let limits = old
            .tunnels
            .iter()
            .chain(new.tunnels.iter())
            .map(|t| &t.sites)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .filter(|[a, b]| old.tunnel_limit(a, b) != new.tunnel_limit(a, b))
            .count();
        let old_local = running.map(|r| &r.local);
        let local = old_local.map_or(0, |l| {
            usize::from(l.self_name != self.local.self_name)
                + usize::from(l.trunk != self.local.trunk)
                + l.tunnel_trunks
                    .keys()
                    .chain(self.local.tunnel_trunks.keys())
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .filter(|k| l.tunnel_trunks.get(*k) != self.local.tunnel_trunks.get(*k))
                    .count()
        });
        settings + sites + maps + limits + local
    }

    /// The rows of the tunnel table: every tunnel of the network, then the
    /// limits of site pairs without a tunnel (so they can be removed).
    pub fn tunnel_rows(&self) -> Vec<TunnelRow> {
        let network = &self.network;
        let mut rows: Vec<TunnelRow> = topology::edges(network)
            .into_iter()
            .map(|(a, b, servers)| TunnelRow {
                carries: topology::tunnel_vlans(network, &a, &b),
                limit: network.tunnel_limit(&a, &b).cloned(),
                entry: network.tunnels.iter().position(|t| t.is(&a, &b)),
                servers: Some(servers),
                sites: [a, b],
            })
            .collect();
        for (i, tunnel) in network.tunnels.iter().enumerate() {
            if !rows.iter().any(|r| r.sites == tunnel.sites) {
                rows.push(TunnelRow {
                    sites: tunnel.sites.clone(),
                    servers: None,
                    carries: None,
                    limit: Some(tunnel.vlans.clone()),
                    entry: Some(i),
                });
            }
        }
        rows
    }

    /// Diff of the draft against the running configuration.
    pub fn diff(&self, running: Option<&Running>) -> Vec<DiffLine> {
        let (old_network, old_local) = running
            .map(|r| (r.network_text(), r.local_text()))
            .unwrap_or_default();
        diff::config_diff(
            (&old_network, &old_local),
            (&self.network_text(), &self.local_text()),
            "running",
            "draft",
        )
    }
}

/// One row of the tunnel table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelRow {
    /// The two sites, the smaller name first.
    pub sites: [String; 2],
    /// Between two servers; `None` if there is no tunnel (only a limit).
    pub servers: Option<bool>,
    /// What the tunnel carries; `None` if there is no tunnel.
    pub carries: Option<VlanSet>,
    /// Its own limit, if it has one.
    pub limit: Option<VlanSet>,
    /// Index of the limit in `network.tunnels`.
    pub entry: Option<usize>,
}

/// True if `path` is `parent` or lies below it.
pub fn is_within(path: &FieldPath, parent: &FieldPath) -> bool {
    let (p, q) = (path.as_str(), parent.as_str());
    p == q || p.strip_prefix(q).is_some_and(|rest| rest.starts_with('.'))
}

fn worst<'a>(diags: impl Iterator<Item = &'a Diagnostic>) -> Option<Severity> {
    let mut found = None;
    for d in diags {
        if d.is_error() {
            return Some(Severity::Error);
        }
        found = Some(Severity::Warning);
    }
    found
}

/// Compares two networks ignoring the revision counter.
pub fn same_network(a: &Network, b: &Network) -> bool {
    a.control_port == b.control_port
        && a.vlans == b.vlans
        && a.mtu_probe_interval == b.mtu_probe_interval
        && a.mss == b.mss
        && a.commit_timeout == b.commit_timeout
        && a.sites == b.sites
        && a.maps == b.maps
        && a.tunnels == b.tunnels
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::fixtures;

    #[test]
    fn running_parses_texts() {
        let running = Running::parse(&fixtures::config_texts()).unwrap();
        assert_eq!(running.revision, 7);
        assert_eq!(running.network.as_ref().unwrap().sites.len(), 6);
        assert_eq!(running.local.self_name.as_deref(), Some("heim"));
    }

    #[test]
    fn draft_tracks_changes_and_diagnostics() {
        let running = Running::parse(&fixtures::config_texts()).unwrap();
        let mut draft = Draft::from_running(&running).unwrap();
        assert!(!draft.differs_from(&running));
        assert_eq!(draft.errors(), 0);
        // A revision bump alone is not a change.
        draft.network.revision += 1;
        assert!(!draft.differs_from(&running));
        // Removing heim leaves "self" pointing at a site that is gone.
        let removed = draft.remove_site(0).unwrap();
        assert_eq!(removed.name, "heim");
        assert!(draft.differs_from(&running));
        assert!(draft.errors() > 0);
        let self_path = FieldPath::local("self");
        assert_eq!(draft.worst_at(&self_path), Some(Severity::Error));
        // An empty VLAN list of a site is marked on the site.
        draft.network.sites[1].vlans = Some(wanderlan_core::config::VlanSet::new());
        draft.revalidate();
        assert_eq!(
            draft.worst_within(&FieldPath::site(1)),
            Some(Severity::Error)
        );
    }

    #[test]
    fn removing_a_site_removes_its_maps() {
        let running = Running::parse(&fixtures::config_texts()).unwrap();
        let mut draft = Draft::from_running(&running).unwrap();
        let before = draft.network.maps.len();
        let index = draft
            .network
            .sites
            .iter()
            .position(|s| s.name == "haus3")
            .unwrap();
        draft.remove_site(index);
        assert_eq!(draft.network.maps.len(), before - 2);
    }

    #[test]
    fn rename_updates_references() {
        let running = Running::parse(&fixtures::config_texts()).unwrap();
        let mut draft = Draft::from_running(&running).unwrap();
        draft
            .local
            .tunnel_trunks
            .insert("haus3".into(), "ens20".into());
        let haus3 = draft
            .network
            .sites
            .iter()
            .position(|s| s.name == "haus3")
            .unwrap();
        draft.network.sites[haus3].name = "haus33".into();
        draft.rename_references("haus3", "haus33");
        draft.network.sites[0].name = "home".into();
        draft.rename_references("heim", "home");
        // buero manages haus6.
        draft.network.sites[1].name = "office".into();
        draft.rename_references("buero", "office");
        draft.revalidate();
        assert_eq!(draft.errors(), 0, "{:?}", draft.diagnostics);
        assert_eq!(draft.local.self_name.as_deref(), Some("home"));
        assert!(draft.network.maps.iter().all(|m| m.site == "haus33"));
        assert_eq!(
            draft.local.tunnel_trunks.get("haus33").map(String::as_str),
            Some("ens20")
        );
        let haus6 = draft.network.site("haus6").unwrap();
        assert_eq!(haus6.managed_by.as_deref(), Some("office"));
    }

    #[test]
    fn within_does_not_match_longer_indices() {
        assert!(is_within(
            &FieldPath::site_field(1, "hub"),
            &FieldPath::site(1)
        ));
        assert!(is_within(&FieldPath::site(1), &FieldPath::site(1)));
        assert!(!is_within(
            &FieldPath::site_field(10, "hub"),
            &FieldPath::site(1)
        ));
    }

    #[test]
    fn tunnel_limits_follow_removed_and_renamed_sites() {
        let running = Running::parse(&fixtures::config_texts()).unwrap();
        let mut draft = Draft::from_running(&running).unwrap();
        draft
            .network
            .set_tunnel_limit("heim", "buero", Some(VlanSet::from_range(20, 20)));
        draft
            .network
            .set_tunnel_limit("haus2", "haus3", Some(VlanSet::new()));
        draft.rename_references("buero", "zentrale");
        assert!(draft.network.tunnel_limit("heim", "zentrale").is_some());
        assert_eq!(
            draft.network.tunnels[0].sites,
            ["heim".to_string(), "zentrale".to_string()],
            "the smaller name first"
        );
        let haus2 = draft
            .network
            .sites
            .iter()
            .position(|s| s.name == "haus2")
            .unwrap();
        draft.remove_site(haus2);
        assert_eq!(draft.network.tunnels.len(), 1);
    }
}
