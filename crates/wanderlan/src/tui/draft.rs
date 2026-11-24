//! The running configuration and the draft the user edits.
//!
//! All edits go into a [`Draft`], a copy of the running configuration. The
//! draft is validated after every change with the same rules the daemon
//! uses; the diagnostics carry [`FieldPath`]s so the views can mark the
//! offending rows and fields.

use wanderlan_core::api::ConfigTexts;
use wanderlan_core::config::{
    Diagnostic, FieldPath, Local, Mesh, Severity, Site, parse_local, parse_mesh, render_local,
    render_mesh, validate,
};

use super::diff::{self, DiffLine};

/// The configuration the daemon runs, as returned by `get_config`.
#[derive(Debug, Clone, PartialEq)]
pub struct Running {
    /// True if the site has a configuration.
    pub configured: bool,
    /// Mesh revision.
    pub revision: u64,
    /// Mesh fingerprint.
    pub fingerprint: String,
    /// The mesh, if there is one.
    pub mesh: Option<Mesh>,
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
        let mesh = if texts.mesh.trim().is_empty() {
            None
        } else {
            parse_mesh(&texts.mesh)
                .map_err(|d| first("mesh.toml", d))?
                .mesh
        };
        let local = parse_local(&texts.local)
            .map_err(|d| first("local.toml", d))?
            .local;
        Ok(Self {
            configured: texts.configured,
            revision: texts.revision,
            fingerprint: texts.fingerprint.clone(),
            mesh,
            local,
        })
    }

    /// Canonical `mesh.toml` (empty if there is no mesh).
    pub fn mesh_text(&self) -> String {
        self.mesh.as_ref().map(render_mesh).unwrap_or_default()
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
    /// The edited mesh.
    pub mesh: Mesh,
    /// The edited local settings.
    pub local: Local,
    /// Result of validating `mesh` and `local`.
    pub diagnostics: Vec<Diagnostic>,
}

impl Draft {
    /// Creates a draft and validates it.
    pub fn new(base_revision: u64, mesh: Mesh, local: Local) -> Self {
        let mut draft = Self {
            base_revision,
            mesh,
            local,
            diagnostics: Vec::new(),
        };
        draft.revalidate();
        draft
    }

    /// A draft of the running configuration, if it has a mesh.
    pub fn from_running(running: &Running) -> Option<Self> {
        running
            .mesh
            .clone()
            .map(|mesh| Self::new(running.revision, mesh, running.local.clone()))
    }

    /// Validates the draft again after a change.
    pub fn revalidate(&mut self) {
        self.diagnostics = validate(&self.mesh, Some(&self.local));
    }

    /// True if the draft differs from the running configuration (the
    /// revision counter is ignored).
    pub fn differs_from(&self, running: &Running) -> bool {
        match &running.mesh {
            None => true,
            Some(mesh) => !same_mesh(&self.mesh, mesh) || self.local != running.local,
        }
    }

    /// Canonical `mesh.toml` of the draft.
    pub fn mesh_text(&self) -> String {
        render_mesh(&self.mesh)
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
        if index >= self.mesh.sites.len() {
            return None;
        }
        let site = self.mesh.sites.remove(index);
        self.mesh.maps.retain(|m| m.site != site.name);
        self.revalidate();
        Some(site)
    }

    /// Removes a VLAN mapping.
    pub fn remove_map(&mut self, index: usize) -> bool {
        if index >= self.mesh.maps.len() {
            return false;
        }
        self.mesh.maps.remove(index);
        self.revalidate();
        true
    }

    /// Updates all references to a renamed site (mappings, `self`).
    pub fn rename_references(&mut self, old: &str, new: &str) {
        for map in &mut self.mesh.maps {
            if map.site == old {
                map.site = new.to_string();
            }
        }
        for site in &mut self.mesh.sites {
            if site.managed_by.as_deref() == Some(old) {
                site.managed_by = Some(new.to_string());
            }
        }
        if self.local.self_name.as_deref() == Some(old) {
            self.local.self_name = Some(new.to_string());
        }
        if let Some(trunk) = self.local.tunnel_trunks.remove(old) {
            self.local.tunnel_trunks.insert(new.to_string(), trunk);
        }
    }

    /// Diff of the draft against the running configuration.
    pub fn diff(&self, running: Option<&Running>) -> Vec<DiffLine> {
        let (old_mesh, old_local) = running
            .map(|r| (r.mesh_text(), r.local_text()))
            .unwrap_or_default();
        diff::config_diff(
            (&old_mesh, &old_local),
            (&self.mesh_text(), &self.local_text()),
            "running",
            "draft",
        )
    }
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

/// Compares two meshes ignoring the revision counter.
pub fn same_mesh(a: &Mesh, b: &Mesh) -> bool {
    a.control_port == b.control_port
        && a.vlans == b.vlans
        && a.mtu_probe_interval == b.mtu_probe_interval
        && a.mss == b.mss
        && a.commit_timeout == b.commit_timeout
        && a.sites == b.sites
        && a.maps == b.maps
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::fixtures;

    #[test]
    fn running_parses_texts() {
        let running = Running::parse(&fixtures::config_texts()).unwrap();
        assert_eq!(running.revision, 7);
        assert_eq!(running.mesh.as_ref().unwrap().sites.len(), 6);
        assert_eq!(running.local.self_name.as_deref(), Some("heim"));
    }

    #[test]
    fn draft_tracks_changes_and_diagnostics() {
        let running = Running::parse(&fixtures::config_texts()).unwrap();
        let mut draft = Draft::from_running(&running).unwrap();
        assert!(!draft.differs_from(&running));
        assert_eq!(draft.errors(), 0);
        // A revision bump alone is not a change.
        draft.mesh.revision += 1;
        assert!(!draft.differs_from(&running));
        // Removing heim leaves "self" pointing at a site that is gone.
        let removed = draft.remove_site(0).unwrap();
        assert_eq!(removed.name, "heim");
        assert!(draft.differs_from(&running));
        assert!(draft.errors() > 0);
        let self_path = FieldPath::local("self");
        assert_eq!(draft.worst_at(&self_path), Some(Severity::Error));
        // An empty VLAN list of a site is marked on the site.
        draft.mesh.sites[1].vlans = Some(wanderlan_core::config::VlanSet::new());
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
        let before = draft.mesh.maps.len();
        let index = draft
            .mesh
            .sites
            .iter()
            .position(|s| s.name == "haus3")
            .unwrap();
        draft.remove_site(index);
        assert_eq!(draft.mesh.maps.len(), before - 2);
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
            .mesh
            .sites
            .iter()
            .position(|s| s.name == "haus3")
            .unwrap();
        draft.mesh.sites[haus3].name = "haus33".into();
        draft.rename_references("haus3", "haus33");
        draft.mesh.sites[0].name = "home".into();
        draft.rename_references("heim", "home");
        // buero manages haus6.
        draft.mesh.sites[1].name = "office".into();
        draft.rename_references("buero", "office");
        draft.revalidate();
        assert_eq!(draft.errors(), 0, "{:?}", draft.diagnostics);
        assert_eq!(draft.local.self_name.as_deref(), Some("home"));
        assert!(draft.mesh.maps.iter().all(|m| m.site == "haus33"));
        assert_eq!(
            draft.local.tunnel_trunks.get("haus33").map(String::as_str),
            Some("ens20")
        );
        let haus6 = draft.mesh.site("haus6").unwrap();
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
}
