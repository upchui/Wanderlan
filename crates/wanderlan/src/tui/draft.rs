//! The running configuration and the draft the user edits.
//!
//! All edits go into a [`Draft`], a copy of the running configuration. The
//! draft is validated after every change with the same rules the daemon
//! uses; the diagnostics carry [`FieldPath`]s so the views can mark the
//! offending rows and fields.

use wanderlan_core::api::ConfigTexts;
use wanderlan_core::config::{
    Diagnostic, FieldPath, Local, LocalVid, Network, Severity, Site, TunnelVlans, VlanSet,
    parse_local, parse_network, render_local, render_network, validate,
};
use wanderlan_core::identity::SiteId;
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
        let local = parse_local(&texts.local, network.as_ref())
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
        render_local(&self.local, self.network.as_ref())
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
    /// The network the draft was copied from (what its edits are against).
    pub base: Option<Box<Network>>,
    /// The `local.toml` the draft was copied from.
    pub base_local: Option<Box<Local>>,
}

impl Draft {
    /// Creates a draft and validates it.
    pub fn new(base_revision: u64, network: Network, local: Local) -> Self {
        let mut draft = Self {
            base_revision,
            network,
            local,
            diagnostics: Vec::new(),
            base: None,
            base_local: None,
        };
        draft.revalidate();
        draft
    }

    /// A draft of the running configuration, if it has a network.
    pub fn from_running(running: &Running) -> Option<Self> {
        running.network.clone().map(|network| {
            let mut draft = Self::new(running.revision, network.clone(), running.local.clone());
            draft.base = Some(Box::new(network));
            draft.base_local = Some(Box::new(running.local.clone()));
            draft
        })
    }

    /// Moves the edits of the draft onto `running`, which changed while the
    /// draft was edited (a sync, another session): per network setting,
    /// site, mapping and tunnel limit, what the draft changed wins, the rest
    /// is what runs now. Returns what both changed (the draft's edit wins).
    pub fn rebase_onto(&mut self, running: &Running) -> Vec<String> {
        let (Some(base), Some(now)) = (self.base.as_deref().cloned(), running.network.clone())
        else {
            self.rebase(running);
            return Vec::new();
        };
        let mine = &self.network;
        let mut both = Vec::new();
        let mut merged = now.clone();
        macro_rules! setting {
            ($field:ident, $label:expr) => {
                if mine.$field != base.$field {
                    if now.$field != base.$field && now.$field != mine.$field {
                        both.push($label.to_string());
                    }
                    merged.$field = mine.$field.clone();
                }
            };
        }
        setting!(vlans, "the VLANs of the network");
        setting!(control_port, "the control port");
        setting!(tunnel_port, "the tunnel port");
        setting!(mtu_probe_interval, "the path MTU check");
        setting!(commit_timeout, "the commit timeout");
        setting!(mss, "the TCP MSS");
        merged.sites = now
            .sites
            .iter()
            .filter_map(|n| match (base.site(n.id), mine.site(n.id)) {
                (Some(b), Some(d)) if d != b => {
                    // (Without the ports: they are taken as they run.)
                    let differs = |x: &Site, y: &Site| {
                        Site {
                            ports: y.ports,
                            ..x.clone()
                        } != *y
                    };
                    if differs(n, b) && differs(n, d) {
                        both.push(format!("site {}", now.label(n.id)));
                    }
                    // The ports of another site are its own, and this site's
                    // are applied at once, never kept in the draft: always
                    // as they run now.
                    let mut site = d.clone();
                    site.ports = n.ports;
                    Some(site)
                }
                // Removed in the draft.
                (Some(_), None) => None,
                _ => Some(n.clone()),
            })
            .collect();
        for site in &mine.sites {
            if base.site(site.id).is_none() && merged.site(site.id).is_none() {
                merged.sites.push(site.clone());
            }
        }
        merged
            .maps
            .retain(|m| !(base.maps.contains(m) && !mine.maps.contains(m)));
        for map in &mine.maps {
            if !base.maps.contains(map) && !merged.maps.contains(map) {
                merged.maps.push(map.clone());
            }
        }
        for t in &base.tunnels {
            let [a, b] = t.sites;
            if mine.tunnel_limit(a, b).is_none() {
                merged.set_tunnel_limit(a, b, None);
            }
        }
        for t in &mine.tunnels {
            let [a, b] = t.sites;
            if base.tunnel_limit(a, b) != Some(&t.vlans) {
                merged.set_tunnel_limit(a, b, Some(t.vlans.clone()));
            }
        }
        merged.revision = now.revision;
        self.network = merged;
        // local.toml the same way, setting by setting.
        if let Some(base_local) = self.base_local.as_deref().cloned() {
            let mine = self.local.clone();
            let mut local = running.local.clone();
            macro_rules! local_setting {
                ($field:ident) => {
                    if mine.$field != base_local.$field {
                        local.$field = mine.$field.clone();
                    }
                };
            }
            local_setting!(self_id);
            local_setting!(trunk);
            local_setting!(tunnel_trunks);
            local_setting!(commit_timeout);
            local_setting!(mtu_probe_interval);
            local_setting!(mss);
            self.local = local;
        }
        self.rebase(running);
        both
    }

    /// Validates the draft again after a change.
    pub fn revalidate(&mut self) {
        self.diagnostics = validate(&self.network, Some(&self.local));
    }

    /// Bases the draft on `running` (a change of it came back after a
    /// rollback, for example).
    pub fn rebase(&mut self, running: &Running) {
        self.base_revision = running.revision;
        self.base = running.network.clone().map(Box::new);
        self.base_local = Some(Box::new(running.local.clone()));
        self.revalidate();
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
        render_local(&self.local, Some(&self.network))
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
        self.network.maps.retain(|m| m.site != site.id);
        self.network.tunnels.retain(|t| !t.touches(site.id));
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

    /// Renames the site `id` to `new`. Everything refers to it by its
    /// identity; a site known by its name (no identity of its own yet) gets
    /// the provisional identity of its new name, everywhere.
    pub fn rename_site(&mut self, id: SiteId, new: &str) {
        let provisional = self.network.site(id).is_some_and(|s| !s.has_identity());
        self.network.rename_site(id, new);
        if provisional {
            self.local.rekey(id, SiteId::provisional(new));
        }
    }

    /// How many things the draft changes against `running`: settings,
    /// sites, mappings, tunnel limits and local settings (an edited mapping
    /// or limit counts once).
    pub fn changes(&self, running: Option<&Running>) -> usize {
        let Some(before) = running.and_then(|r| r.network.as_ref()) else {
            return 0;
        };
        let new = &self.network;
        // A site of an older version renamed is one change of its site,
        // though its provisional identity changes.
        let (base, _) = wanderlan_core::rename::rekeyed(before, new);
        let old = &base;
        let settings = [
            old.vlans != new.vlans,
            old.control_port != new.control_port,
            old.tunnel_port != new.tunnel_port,
            old.mtu_probe_interval != new.mtu_probe_interval,
            old.mss != new.mss,
            old.commit_timeout != new.commit_timeout,
        ]
        .iter()
        .filter(|c| **c)
        .count();
        // A site of an older version renamed got another (provisional)
        // identity: still one change.
        let sites = new
            .sites
            .iter()
            .filter(|s| {
                old.site(s.id) != Some(*s)
                    || (before.site(s.id).is_none() && old.site(s.id).is_some())
            })
            .count()
            + old
                .sites
                .iter()
                .filter(|s| new.site(s.id).is_none())
                .count();
        let added = new.maps.iter().filter(|m| !old.maps.contains(m)).count();
        let removed = old.maps.iter().filter(|m| !new.maps.contains(m)).count();
        let maps = added.max(removed);
        let limits = old
            .tunnels
            .iter()
            .chain(new.tunnels.iter())
            .map(|t| t.sites)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .filter(|[a, b]| old.tunnel_limit(*a, *b) != new.tunnel_limit(*a, *b))
            .count();
        let old_local = running.map(|r| &r.local);
        let local = old_local.map_or(0, |l| {
            usize::from(l.self_id != self.local.self_id)
                + usize::from(l.trunk != self.local.trunk)
                + usize::from(l.commit_timeout != self.local.commit_timeout)
                + usize::from(l.mtu_probe_interval != self.local.mtu_probe_interval)
                + usize::from(l.mss != self.local.mss)
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
        let mut rows = self.tunnel_rows_by_id();
        // Shown by name: each pair and the rows.
        let network = &self.network;
        for row in &mut rows {
            let [a, b] = row.sites;
            if network.label(b) < network.label(a) {
                row.sites = [b, a];
            }
        }
        rows.sort_by_cached_key(|r| {
            (
                r.servers.is_none(),
                network.label(r.sites[0]),
                network.label(r.sites[1]),
            )
        });
        rows
    }

    fn tunnel_rows_by_id(&self) -> Vec<TunnelRow> {
        let network = &self.network;
        let mut rows: Vec<TunnelRow> = topology::edges(network)
            .into_iter()
            .map(|(a, b, servers)| TunnelRow {
                carries: topology::tunnel_vlans(network, a, b),
                limit: network.tunnel_limit(a, b).cloned(),
                entry: network.tunnels.iter().position(|t| t.is(a, b)),
                servers: Some(servers),
                sites: [a, b],
            })
            .collect();
        for (i, tunnel) in network.tunnels.iter().enumerate() {
            if !rows.iter().any(|r| r.sites == tunnel.sites) {
                rows.push(TunnelRow {
                    sites: tunnel.sites,
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
    pub sites: [SiteId; 2],
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

/// Compares two networks ignoring the revision counter and the order of
/// the mappings and tunnel limits (the daemon may return them in another).
pub fn same_network(a: &Network, b: &Network) -> bool {
    fn maps(n: &Network) -> Vec<(SiteId, u16, LocalVid)> {
        let mut list: Vec<_> = n.maps.iter().map(|m| (m.site, m.shared, m.local)).collect();
        list.sort_unstable();
        list
    }
    fn tunnels(n: &Network) -> Vec<&TunnelVlans> {
        let mut list: Vec<_> = n.tunnels.iter().collect();
        list.sort_unstable_by_key(|x| x.sites);
        list
    }
    a.control_port == b.control_port
        && a.tunnel_port == b.tunnel_port
        && a.vlans == b.vlans
        && a.mtu_probe_interval == b.mtu_probe_interval
        && a.mss == b.mss
        && a.commit_timeout == b.commit_timeout
        && a.sites == b.sites
        && maps(a) == maps(b)
        && tunnels(a) == tunnels(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::fixtures;
    use crate::tui::fixtures::id;

    #[test]
    fn running_parses_texts() {
        let running = Running::parse(&fixtures::config_texts()).unwrap();
        assert_eq!(running.revision, 7);
        assert_eq!(running.network.as_ref().unwrap().sites.len(), 6);
        assert_eq!(running.local.self_id, Some(id("heim")));
    }

    #[test]
    fn a_draft_moves_onto_a_configuration_that_changed_meanwhile() {
        let running = Running::parse(&fixtures::config_texts()).unwrap();
        let mut draft = Draft::from_running(&running).unwrap();
        // Here: haus3 gets VLAN 20 only. Elsewhere (a sync): buero goes
        // offline, revision 8.
        draft
            .network
            .sites
            .iter_mut()
            .find(|s| s.name == "haus3")
            .unwrap()
            .vlans = Some(VlanSet::from_range(20, 20));
        let mut newer = running.clone();
        newer.revision = 8;
        let net = newer.network.as_mut().unwrap();
        net.revision = 8;
        net.sites
            .iter_mut()
            .find(|s| s.name == "buero")
            .unwrap()
            .online = false;
        assert!(draft.rebase_onto(&newer).is_empty());
        assert_eq!(draft.base_revision, 8);
        let n = &draft.network;
        assert!(
            !n.by_name("buero").unwrap().online,
            "the other change stays"
        );
        assert_eq!(
            n.by_name("haus3").unwrap().vlans,
            Some(VlanSet::from_range(20, 20))
        );
        // The diff shows only the own edit now.
        assert!(draft.differs_from(&newer));

        // Another site's ports and identity are as they run; local.toml
        // setting by setting.
        let mut draft = Draft::from_running(&running).unwrap();
        for s in &mut draft.network.sites {
            if s.name == "buero" {
                s.vlans = Some(VlanSet::from_range(30, 30));
            }
        }
        draft.local.mss = Some(1300);
        let mut newer = running.clone();
        newer.revision = 8;
        newer.local.commit_timeout = Some(std::time::Duration::from_secs(120));
        for s in &mut newer.network.as_mut().unwrap().sites {
            if s.name == "buero" {
                s.ports.control = Some(52821);
            }
        }
        draft.rebase_onto(&newer);
        let buero = draft.network.by_name("buero").unwrap();
        assert_eq!(buero.ports.control, Some(52821));
        assert_eq!(buero.vlans, Some(VlanSet::from_range(30, 30)));
        assert_eq!(draft.local.mss, Some(1300));
        assert_eq!(
            draft.local.commit_timeout,
            Some(std::time::Duration::from_secs(120))
        );
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
            .insert(id("haus3"), vec!["ens20".into()]);
        let haus3 = draft
            .network
            .sites
            .iter()
            .position(|s| s.name == "haus3")
            .unwrap();
        draft.network.sites[haus3].name = "haus33".into();
        draft.rename_site(id("haus3"), "haus33");
        draft.network.sites[0].name = "home".into();
        draft.rename_site(id("heim"), "home");
        draft.network.sites[1].name = "office".into();
        draft.rename_site(id("buero"), "office");
        draft.revalidate();
        assert_eq!(draft.errors(), 0, "{:?}", draft.diagnostics);
        assert_eq!(draft.local.self_id, Some(id("home")));
        assert!(draft.network.maps.iter().all(|m| m.site == id("haus33")));
        assert_eq!(
            draft
                .local
                .tunnel_trunks
                .get(&id("haus33"))
                .and_then(|p| p.first())
                .map(String::as_str),
            Some("ens20")
        );
        let haus6 = draft.network.by_name("haus6").unwrap();
        assert_eq!(haus6.uplink, Some(id("home")));
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
            .set_tunnel_limit(id("heim"), id("buero"), Some(VlanSet::from_range(20, 20)));
        draft
            .network
            .set_tunnel_limit(id("haus2"), id("haus3"), Some(VlanSet::new()));
        draft.rename_site(id("buero"), "zentrale");
        assert!(
            draft
                .network
                .tunnel_limit(id("heim"), id("zentrale"))
                .is_some()
        );
        assert_eq!(
            draft.network.tunnels[0].sites,
            wanderlan_core::config::site_pair(id("heim"), id("zentrale")),
            "the smaller identity first"
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
