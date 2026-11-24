//! Configuration: `mesh.toml` (shared by all sites) and `local.toml`.
//!
//! Loading happens in two steps: [`parse_mesh`]/[`parse_local`] turn the TOML
//! text into the typed model (reporting syntax and type errors with line
//! numbers), then [`validate`] checks the semantic rules. [`load`] combines
//! both and attaches line numbers to all diagnostics.

mod diag;
mod model;
mod raw;
mod render;
mod validate;
mod vlan;

pub use diag::{ConfigFile, Diagnostic, FieldPath, LineIndex, Severity, SpanMap, has_errors};
pub use model::*;
pub use raw::{ParsedLocal, ParsedMesh, parse_local, parse_mesh};
pub use render::{fingerprint, render_local, render_mesh, unified_diff};
pub use validate::{
    COMMIT_TIMEOUT_RANGE, IFNAME_MAX, MSS_MAX, MSS_MIN, PROBE_INTERVAL_MIN, ifname_error,
    site_name_error, validate,
};
pub use vlan::{VID_MAX, VID_MIN, VlanSet, VlanSetError};

/// A complete, valid configuration of a configured site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The shared mesh configuration.
    pub mesh: Mesh,
    /// The local configuration; `self_name` and `trunk` are guaranteed set.
    pub local: Local,
}

impl Config {
    /// Name of the local site.
    pub fn self_name(&self) -> &str {
        self.local.self_name.as_deref().unwrap_or_default()
    }

    /// The local site entry.
    pub fn self_site(&self) -> Option<&Site> {
        self.mesh.site(self.self_name())
    }

    /// Name of the trunk interface.
    pub fn trunk(&self) -> &str {
        self.local.trunk.as_deref().unwrap_or_default()
    }
}

/// What was found on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Loaded {
    /// `local.toml` has no `self` yet: the site waits for the setup wizard.
    Unconfigured {
        /// Local settings found so far.
        local: Local,
        /// A mesh, if `mesh.toml` already contains one.
        mesh: Option<Mesh>,
    },
    /// A complete, valid configuration.
    Configured(Config),
}

/// Successful load result with non-fatal warnings.
#[derive(Debug, Clone)]
pub struct LoadOutcome {
    /// The loaded configuration.
    pub loaded: Loaded,
    /// Warnings with line numbers.
    pub warnings: Vec<Diagnostic>,
}

/// Parses and validates both files. On failure, returns all diagnostics
/// (errors and warnings) with line numbers.
pub fn load(mesh_text: &str, local_text: &str) -> Result<LoadOutcome, Vec<Diagnostic>> {
    let parsed_mesh = parse_mesh(mesh_text);
    let parsed_local = parse_local(local_text);
    let (parsed_mesh, parsed_local) = match (parsed_mesh, parsed_local) {
        (Ok(m), Ok(l)) => (m, l),
        (m, l) => {
            let mut diags = Vec::new();
            if let Err(e) = m {
                diags.extend(e);
            }
            if let Err(e) = l {
                diags.extend(e);
            }
            return Err(diags);
        }
    };

    let local = parsed_local.local;
    let mut diags = match (&parsed_mesh.mesh, local.is_configured()) {
        (None, false) => Vec::new(),
        (None, true) => vec![Diagnostic::error(
            ConfigFile::Mesh,
            FieldPath::root(),
            "mesh.toml contains no mesh configuration, but local.toml sets `self`",
        )],
        (Some(mesh), _) => validate(mesh, Some(&local)),
    };
    diags.extend(parsed_mesh.warnings.iter().cloned());
    parsed_mesh.spans.locate(ConfigFile::Mesh, &mut diags);
    parsed_local.spans.locate(ConfigFile::Local, &mut diags);

    if has_errors(&diags) {
        return Err(diags);
    }
    let loaded = match (parsed_mesh.mesh, local.is_configured()) {
        (Some(mesh), true) => Loaded::Configured(Config { mesh, local }),
        (mesh, _) => Loaded::Unconfigured { local, mesh },
    };
    Ok(LoadOutcome {
        loaded,
        warnings: diags,
    })
}

/// Validates a `mesh.toml` text on its own, optionally together with a
/// `local.toml` text. Returns all diagnostics with line numbers; the
/// configuration is valid if none of them is an error.
pub fn check(mesh_text: &str, local_text: Option<&str>) -> Vec<Diagnostic> {
    match local_text {
        Some(local_text) => match load(mesh_text, local_text) {
            Ok(outcome) => outcome.warnings,
            Err(diags) => diags,
        },
        None => match parse_mesh(mesh_text) {
            Err(diags) => diags,
            Ok(parsed) => match &parsed.mesh {
                None => vec![Diagnostic::error(
                    ConfigFile::Mesh,
                    FieldPath::root(),
                    "the file contains no mesh configuration",
                )],
                Some(mesh) => {
                    let mut diags = validate(mesh, None);
                    parsed.spans.locate(ConfigFile::Mesh, &mut diags);
                    diags.extend(parsed.warnings.iter().cloned());
                    diags
                }
            },
        },
    }
}
