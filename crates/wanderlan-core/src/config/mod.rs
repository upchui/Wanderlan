//! Configuration: `network.toml` (shared by all sites) and `local.toml`.
//!
//! Loading happens in two steps: [`parse_network`]/[`parse_local`] turn the TOML
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
pub use raw::{
    ParsedLocal, ParsedNetwork, local_uses_names, parse_local, parse_network, parse_network_in,
};
pub use render::{
    disk_format, expressible_by_names, fingerprint, local_fingerprint, render_local,
    render_network, render_network_wire, unified_diff,
};
pub use validate::{
    COMMIT_TIMEOUT_RANGE, IFNAME_MAX, MSS_MAX, MSS_MIN, PROBE_INTERVAL_MIN, ifname_error,
    site_name_error, validate,
};
pub use vlan::{NATIVE_VID, VID_MAX, VID_MIN, VlanSet, VlanSetError};

/// A complete, valid configuration of a configured site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The shared network configuration.
    pub network: Network,
    /// The local configuration; `self_id` and `trunk` are guaranteed set.
    pub local: Local,
}

impl Config {
    /// Identity of the local site.
    pub fn self_id(&self) -> crate::identity::SiteId {
        self.local
            .self_id
            .unwrap_or_else(|| crate::identity::SiteId::provisional(""))
    }

    /// Name of the local site.
    pub fn self_name(&self) -> &str {
        self.self_site().map_or("", |s| s.name.as_str())
    }

    /// The local site entry.
    pub fn self_site(&self) -> Option<&Site> {
        self.network.site(self.self_id())
    }

    /// Time to confirm a change here: this site's, or the network's.
    pub fn commit_timeout(&self) -> std::time::Duration {
        self.local
            .commit_timeout
            .unwrap_or(self.network.commit_timeout)
    }

    /// How often this site measures the path MTU.
    pub fn mtu_probe_interval(&self) -> std::time::Duration {
        self.local
            .mtu_probe_interval
            .unwrap_or(self.network.mtu_probe_interval)
    }

    /// The fixed TCP MSS of this site, if any.
    pub fn mss(&self) -> Option<u16> {
        self.local.mss.or(self.network.mss)
    }

    /// The control port of this site.
    pub fn control_port(&self) -> u16 {
        self.network.control_port_of(self.self_id())
    }

    /// The tunnel port of this site.
    pub fn tunnel_port(&self) -> u16 {
        self.network.tunnel_port_of(self.self_id())
    }

    /// Name of the main trunk port.
    pub fn trunk(&self) -> &str {
        self.local.main_trunk().unwrap_or_default()
    }

    /// The trunk ports of the tunnel to `peer`.
    pub fn trunk_ports_for(&self, peer: crate::identity::SiteId) -> &[String] {
        self.local.trunk_ports_for(peer)
    }

    /// The trunk of the tunnel to `peer` (its own, or the default trunk).
    pub fn trunk_for(&self, peer: crate::identity::SiteId) -> &str {
        self.local.trunk_for(peer).unwrap_or_default()
    }
}

/// What was found on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Loaded {
    /// `local.toml` has no `self` yet: the site waits for the setup wizard.
    Unconfigured {
        /// Local settings found so far.
        local: Local,
        /// A network, if `network.toml` already contains one.
        network: Option<Network>,
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
pub fn load(network_text: &str, local_text: &str) -> Result<LoadOutcome, Vec<Diagnostic>> {
    let parsed_network = parse_network(network_text);
    let parsed_local = parse_local(
        local_text,
        parsed_network
            .as_ref()
            .ok()
            .and_then(|p| p.network.as_ref()),
    );
    let (parsed_network, parsed_local) = match (parsed_network, parsed_local) {
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
    let mut diags = match (&parsed_network.network, local.is_configured()) {
        (None, false) => Vec::new(),
        (None, true) => vec![Diagnostic::error(
            ConfigFile::Network,
            FieldPath::root(),
            "network.toml contains no network configuration, but local.toml sets `self`",
        )],
        (Some(network), _) => validate(network, Some(&local)),
    };
    diags.extend(parsed_network.warnings.iter().cloned());
    parsed_network.spans.locate(ConfigFile::Network, &mut diags);
    parsed_local.spans.locate(ConfigFile::Local, &mut diags);

    if has_errors(&diags) {
        return Err(diags);
    }
    let loaded = match (parsed_network.network, local.is_configured()) {
        (Some(network), true) => Loaded::Configured(Config { network, local }),
        (network, _) => Loaded::Unconfigured { local, network },
    };
    Ok(LoadOutcome {
        loaded,
        warnings: diags,
    })
}

/// Validates a `network.toml` text on its own, optionally together with a
/// `local.toml` text. Returns all diagnostics with line numbers; the
/// configuration is valid if none of them is an error.
pub fn check(network_text: &str, local_text: Option<&str>) -> Vec<Diagnostic> {
    match local_text {
        Some(local_text) => match load(network_text, local_text) {
            Ok(outcome) => outcome.warnings,
            Err(diags) => diags,
        },
        None => match parse_network(network_text) {
            Err(diags) => diags,
            Ok(parsed) => match &parsed.network {
                None => vec![Diagnostic::error(
                    ConfigFile::Network,
                    FieldPath::root(),
                    "the file contains no network configuration",
                )],
                Some(network) => {
                    let mut diags = validate(network, None);
                    parsed.spans.locate(ConfigFile::Network, &mut diags);
                    diags.extend(parsed.warnings.iter().cloned());
                    diags
                }
            },
        },
    }
}
