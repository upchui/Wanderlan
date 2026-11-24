//! Validated, typed configuration model.
//!
//! The model is produced by [`super::parse_network`] and [`super::parse_local`]
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

/// The shared network configuration (`network.toml`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Network {
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
    /// All sites of the network.
    pub sites: Vec<Site>,
    /// Per-site VLAN mappings.
    pub maps: Vec<VlanMap>,
    /// VLAN limits of single tunnels (`[[tunnel]]`).
    pub tunnels: Vec<TunnelVlans>,
}

impl Network {
    /// Creates a network with default settings and no sites.
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
            tunnels: Vec::new(),
        }
    }

    /// The VLAN limit of the tunnel between `a` and `b`, if it has one.
    pub fn tunnel_limit(&self, a: &str, b: &str) -> Option<&VlanSet> {
        self.tunnels.iter().find(|t| t.is(a, b)).map(|t| &t.vlans)
    }

    /// Sets (`Some`) or removes (`None`) the VLAN limit of the tunnel
    /// between `a` and `b`; an existing entry keeps its place.
    pub fn set_tunnel_limit(&mut self, a: &str, b: &str, vlans: Option<VlanSet>) {
        match vlans {
            None => self.tunnels.retain(|t| !t.is(a, b)),
            Some(vlans) => match self.tunnels.iter_mut().find(|t| t.is(a, b)) {
                Some(entry) => entry.vlans = vlans,
                None => self.tunnels.push(TunnelVlans::new(a, b, vlans)),
            },
        }
    }

    /// Looks up a site by name.
    pub fn site(&self, name: &str) -> Option<&Site> {
        self.sites.iter().find(|s| s.name == name)
    }

    /// Shared VLANs a site takes part in: its own list, or the network default.
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

    /// The site's own VLANs: on its trunk and towards its own clients only,
    /// never towards its uplink or other servers. Without the native VLAN if
    /// the site puts its untagged traffic into another VLAN.
    pub fn own_vlans(&self, site: &Site) -> VlanSet {
        let mut vlans = site.own_vlans.clone().unwrap_or_default();
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

/// One site of the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Site {
    /// Unique name, also used in interface names (`gt-<name>`).
    pub name: String,
    /// Address of the site in the existing transport network (e.g. its VPN
    /// address). GRETAP tunnels run between these addresses.
    pub address: Ipv4Addr,
    /// Shared VLANs this site takes part in, like the allowed VLANs of a
    /// switch port; `None` means the network default (`network.vlans`).
    pub vlans: Option<VlanSet>,
    /// VLANs of the site's own, set by the site itself: on its trunk and
    /// towards its own clients, never towards its uplink.
    pub own_vlans: Option<VlanSet>,
    /// False while the site is offline: all its tunnels stay down (the
    /// control channel still works). New sites joining with an invitation
    /// start offline until a user takes them online.
    pub online: bool,
    /// The site that invited this one; only it may change the rules of this
    /// site (VLANs, mappings, online). `None`: every site may.
    pub managed_by: Option<String>,
    /// The site may also change its own rules.
    pub self_managed: bool,
    /// The site this one is plugged into (a client of it): its only tunnel
    /// leads there. `None`: a server, connected with all other servers.
    pub uplink: Option<String>,
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

/// The VLANs one tunnel carries at most (`[[tunnel]]`): a limit on top of
/// the VLANs of its two sites.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TunnelVlans {
    /// The two sites, the smaller name first.
    pub sites: [String; 2],
    /// The VLANs; empty: none at all.
    pub vlans: VlanSet,
}

impl TunnelVlans {
    /// The limit of the tunnel between `a` and `b`.
    pub fn new(a: &str, b: &str, vlans: VlanSet) -> Self {
        Self {
            sites: site_pair(a, b),
            vlans,
        }
    }

    /// True for the tunnel between `a` and `b` (in any order).
    pub fn is(&self, a: &str, b: &str) -> bool {
        self.sites == site_pair(a, b)
    }

    /// True if `site` is one of its ends.
    pub fn touches(&self, site: &str) -> bool {
        self.sites.iter().any(|s| s == site)
    }
}

/// Two site names, the smaller one first.
pub fn site_pair(a: &str, b: &str) -> [String; 2] {
    if a <= b {
        [a.to_string(), b.to_string()]
    } else {
        [b.to_string(), a.to_string()]
    }
}

/// The site-local configuration (`local.toml`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Local {
    /// Name of this site in `network.toml`. `None` means "not configured yet".
    pub self_name: Option<String>,
    /// Local trunk interface that is added to the bridge.
    pub trunk: Option<String>,
    /// Trunks of single tunnels (peer site → interface). Each such trunk is
    /// a segment of its own at this site (own bridge); all other tunnels use
    /// `trunk`.
    pub tunnel_trunks: std::collections::BTreeMap<String, String>,
}

impl Local {
    /// The trunk of the tunnel to `peer`.
    pub fn trunk_for(&self, peer: &str) -> Option<&str> {
        self.tunnel_trunks
            .get(peer)
            .map(String::as_str)
            .or(self.trunk.as_deref())
    }

    /// Returns true if this site has been configured (`self` is set).
    pub fn is_configured(&self) -> bool {
        self.self_name.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_lookups_and_site_vlans() {
        let mut network = Network::new(VlanSet::from_range(10, 20));
        network.sites.push(Site {
            name: "a".into(),
            address: Ipv4Addr::new(10, 0, 0, 1),
            vlans: None,
            own_vlans: None,
            online: true,
            managed_by: None,
            self_managed: false,
            uplink: None,
        });
        network.sites.push(Site {
            name: "b".into(),
            address: Ipv4Addr::new(10, 0, 0, 2),
            vlans: Some(VlanSet::from_range(14, 14)),
            own_vlans: None,
            online: true,
            managed_by: None,
            self_managed: false,
            uplink: None,
        });
        assert!(network.site("c").is_none());
        let a = network.site("a").unwrap();
        let b = network.site("b").unwrap();
        assert_eq!(network.vlans_of(a).len(), 11);
        assert_eq!(network.vlans_of(b).iter().collect::<Vec<_>>(), [14]);
    }
}
