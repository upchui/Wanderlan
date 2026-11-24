//! Validated, typed configuration model.
//!
//! The model is produced by [`super::parse_mesh`] and [`super::parse_local`]
//! and checked by [`super::validate`]. It is also what the TUI edits; the
//! canonical TOML form is produced by [`super::render`].
//!
//! wanderlan does not build a VPN. Every site is reachable at its `address`
//! through a transport network that already exists (usually a VPN set up
//! beforehand); wanderlan only builds the layer-2 part on top of it.

use std::fmt;
use std::net::Ipv4Addr;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::vlan::VlanSet;

/// Default port of the daemon-to-daemon control channel (UDP probes, TCP sync).
pub const DEFAULT_CONTROL_PORT: u16 = 51821;
/// Default interval between path MTU measurements.
pub const DEFAULT_MTU_PROBE_INTERVAL: Duration = Duration::from_secs(300);
/// Default time to confirm a configuration change before it is rolled back.
pub const DEFAULT_COMMIT_TIMEOUT: Duration = Duration::from_secs(60);
/// Maximum length of a site name (`gt-` + name must fit into IFNAMSIZ - 1).
pub const MAX_SITE_NAME_LEN: usize = 12;

/// The shared mesh configuration (`mesh.toml`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mesh {
    /// Revision counter, maintained by the daemon and used for config sync.
    pub revision: u64,
    /// Daemon control port (UDP probes, TCP config sync) on the site addresses.
    pub control_port: u16,
    /// VLANs that are stretched across all sites.
    pub vlans: VlanSet,
    /// Interval between path MTU measurements.
    pub mtu_probe_interval: Duration,
    /// Fixed MSS instead of measuring.
    pub mss: Option<u16>,
    /// Time to confirm a change before it is rolled back.
    pub commit_timeout: Duration,
    /// All sites of the mesh.
    pub sites: Vec<Site>,
    /// Per-site VLAN mappings.
    pub maps: Vec<VlanMap>,
}

impl Mesh {
    /// Creates a mesh with default settings and no sites.
    pub fn new(vlans: VlanSet) -> Self {
        Self {
            revision: 0,
            control_port: DEFAULT_CONTROL_PORT,
            vlans,
            mtu_probe_interval: DEFAULT_MTU_PROBE_INTERVAL,
            mss: None,
            commit_timeout: DEFAULT_COMMIT_TIMEOUT,
            sites: Vec::new(),
            maps: Vec::new(),
        }
    }

    /// Looks up a site by name.
    pub fn site(&self, name: &str) -> Option<&Site> {
        self.sites.iter().find(|s| s.name == name)
    }

    /// Returns all hub sites.
    pub fn hubs(&self) -> impl Iterator<Item = &Site> {
        self.sites.iter().filter(|s| s.role == Role::Hub)
    }

    /// Returns the spokes attached to the given hub.
    pub fn spokes_of<'a>(&'a self, hub: &'a str) -> impl Iterator<Item = &'a Site> + 'a {
        self.sites
            .iter()
            .filter(move |s| s.role == Role::Spoke && s.hub.as_deref() == Some(hub))
    }

    /// Returns the VLAN mappings of one site.
    pub fn maps_of<'a>(&'a self, site: &'a str) -> impl Iterator<Item = &'a VlanMap> + 'a {
        self.maps.iter().filter(move |m| m.site == site)
    }
}

/// Role of a site in the layer-2 topology.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Tunnels to all other hubs and to its own spokes.
    Hub,
    /// Exactly one tunnel, to its hub.
    Spoke,
}

impl Role {
    /// Lowercase name as used in the configuration file.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hub => "hub",
            Self::Spoke => "spoke",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Role {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "hub" => Ok(Self::Hub),
            "spoke" => Ok(Self::Spoke),
            other => Err(format!(
                "unknown role \"{other}\" (expected \"hub\" or \"spoke\")"
            )),
        }
    }
}

/// One site of the mesh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Site {
    /// Unique name, also used in interface names (`gt-<name>`).
    pub name: String,
    /// Hub or spoke.
    pub role: Role,
    /// Name of the hub this spoke connects to (spokes only).
    pub hub: Option<String>,
    /// Address of the site in the existing transport network (e.g. its VPN
    /// address). GRETAP tunnels run between these addresses.
    pub address: Ipv4Addr,
}

/// The local side of a VLAN mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LocalVid {
    /// Untagged traffic on the local trunk.
    Native,
    /// A local VLAN ID that is translated.
    Vid(u16),
}

impl fmt::Display for LocalVid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Native => f.write_str("native"),
            Self::Vid(v) => write!(f, "{v}"),
        }
    }
}

/// A per-site VLAN mapping (`[[map]]`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VlanMap {
    /// Site the mapping applies to.
    pub site: String,
    /// Local side: untagged or a local VLAN ID.
    pub local: LocalVid,
    /// VLAN ID in the shared network.
    pub shared: u16,
}

/// The site-local configuration (`local.toml`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Local {
    /// Name of this site in `mesh.toml`. `None` means "not configured yet".
    pub self_name: Option<String>,
    /// Local trunk interface that is added to the bridge.
    pub trunk: Option<String>,
}

impl Local {
    /// Returns true if this site has been configured (`self` is set).
    pub fn is_configured(&self) -> bool {
        self.self_name.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_parsing() {
        assert_eq!("hub".parse::<Role>(), Ok(Role::Hub));
        assert_eq!("spoke".parse::<Role>(), Ok(Role::Spoke));
        assert!("router".parse::<Role>().is_err());
    }

    #[test]
    fn mesh_lookups() {
        let mut mesh = Mesh::new(VlanSet::from_range(10, 20));
        mesh.sites.push(Site {
            name: "a".into(),
            role: Role::Hub,
            hub: None,
            address: Ipv4Addr::new(10, 0, 0, 1),
        });
        mesh.sites.push(Site {
            name: "b".into(),
            role: Role::Spoke,
            hub: Some("a".into()),
            address: Ipv4Addr::new(10, 0, 0, 2),
        });
        assert_eq!(mesh.hubs().count(), 1);
        assert_eq!(
            mesh.spokes_of("a")
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["b"]
        );
        assert!(mesh.site("c").is_none());
    }
}
