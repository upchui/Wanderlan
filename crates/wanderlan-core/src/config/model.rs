//! Validated, typed configuration model.
//!
//! The model is produced by [`super::parse_mesh`] and [`super::parse_local`]
//! and checked by [`super::validate`]. It is also what the TUI edits; the
//! canonical TOML form is produced by [`super::render`].
//!
//! wanderlan does not build a VPN. Every site is reachable at its `address`
//! through a transport network that already exists (usually a VPN set up
//! beforehand); wanderlan only builds the layer-2 part on top of it. All
//! sites together act as one virtual switch: every site has a tunnel to
//! every other site.

use std::fmt;
use std::net::Ipv4Addr;
use std::time::Duration;

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
    /// VLANs of every site that does not list its own (`[[site]] vlans`).
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

    /// Shared VLANs a site takes part in: its own list, or the mesh default.
    pub fn vlans_of<'a>(&'a self, site: &'a Site) -> &'a VlanSet {
        site.vlans.as_ref().unwrap_or(&self.vlans)
    }

    /// VLANs a site actually exchanges with the other sites: its VLANs,
    /// without the native VLAN if the site puts its untagged traffic into
    /// another VLAN (a native mapping).
    pub fn effective_vlans(&self, site: &Site) -> VlanSet {
        let mut vlans = self.vlans_of(site).clone();
        if self
            .maps_of(&site.name)
            .any(|m| m.local == LocalVid::Native)
        {
            vlans.remove(super::vlan::NATIVE_VID);
        }
        vlans
    }

    /// Returns the VLAN mappings of one site.
    pub fn maps_of<'a>(&'a self, site: &'a str) -> impl Iterator<Item = &'a VlanMap> + 'a {
        self.maps.iter().filter(move |m| m.site == site)
    }
}

/// One site of the mesh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Site {
    /// Unique name, also used in interface names (`gt-<name>`).
    pub name: String,
    /// Address of the site in the existing transport network (e.g. its VPN
    /// address). GRETAP tunnels run between these addresses.
    pub address: Ipv4Addr,
    /// Shared VLANs this site takes part in, like the allowed VLANs of a
    /// switch port; `None` means the mesh default (`mesh.vlans`).
    pub vlans: Option<VlanSet>,
    /// False while the site is offline: all its tunnels stay down (the
    /// control channel still works). New sites joining with an invitation
    /// start offline until a user takes them online.
    pub online: bool,
    /// The site that invited this one; only it may change the rules of this
    /// site (VLANs, mappings, online). `None`: every site may.
    pub managed_by: Option<String>,
    /// The site may also change its own rules.
    pub self_managed: bool,
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
    fn mesh_lookups_and_site_vlans() {
        let mut mesh = Mesh::new(VlanSet::from_range(10, 20));
        mesh.sites.push(Site {
            name: "a".into(),
            address: Ipv4Addr::new(10, 0, 0, 1),
            vlans: None,
            online: true,
            managed_by: None,
            self_managed: false,
        });
        mesh.sites.push(Site {
            name: "b".into(),
            address: Ipv4Addr::new(10, 0, 0, 2),
            vlans: Some(VlanSet::from_range(14, 14)),
            online: true,
            managed_by: None,
            self_managed: false,
        });
        assert!(mesh.site("c").is_none());
        let a = mesh.site("a").unwrap();
        let b = mesh.site("b").unwrap();
        assert_eq!(mesh.vlans_of(a).len(), 11);
        assert_eq!(mesh.vlans_of(b).iter().collect::<Vec<_>>(), [14]);
    }
}
