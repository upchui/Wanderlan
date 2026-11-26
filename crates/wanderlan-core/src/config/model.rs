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
use crate::identity::SiteId;

/// Default port of the daemon-to-daemon control channel (UDP probes, TCP sync).
pub const DEFAULT_CONTROL_PORT: u16 = 51821;
/// Default interval between path MTU measurements.
pub const DEFAULT_MTU_PROBE_INTERVAL: Duration = Duration::from_secs(300);
/// Default time to confirm a configuration change before it is rolled back.
pub const DEFAULT_COMMIT_TIMEOUT: Duration = Duration::from_secs(60);
/// Largest network revision (network.toml holds an i64).
pub const MAX_REVISION: u64 = i64::MAX as u64;

/// The revision after `revision`, at most [`MAX_REVISION`].
pub fn next_revision(revision: u64) -> u64 {
    revision.saturating_add(1).min(MAX_REVISION)
}
/// Maximum length of a site name (`vx-` + name must fit into IFNAMSIZ - 1).
pub const MAX_SITE_NAME_LEN: usize = 12;

/// The shared network configuration (`network.toml`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Network {
    /// Revision counter, maintained by the daemon and used for config sync.
    pub revision: u64,
    /// Daemon control port (UDP probes, TCP config sync) on the site addresses.
    pub control_port: u16,
    /// UDP port of the tunnels between a client and its server (GRE in UDP,
    /// through NAT); `None`: the port after the control port.
    pub tunnel_port: Option<u16>,
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
    /// Puts the mappings and the tunnel limits in a fixed order: by site (in
    /// the order of the sites, unknown ones last), then by VLAN. Their order
    /// has no meaning, but the text has one: the fingerprint is taken over
    /// this form, so that two sites holding the same entries agree.
    pub fn canonicalize(&mut self) {
        let names: Vec<String> = self.sites.iter().map(|s| s.name.clone()).collect();
        let position = |name: &str| names.iter().position(|n| n == name).unwrap_or(usize::MAX);
        self.maps
            .sort_by_cached_key(|m| (position(&m.site), m.shared, m.local));
        self.tunnels.sort_by_cached_key(|t| {
            let (a, b) = (position(&t.sites[0]), position(&t.sites[1]));
            (a.min(b), a.max(b), t.sites.clone())
        });
    }

    /// Creates a network with default settings and no sites.
    pub fn new(vlans: VlanSet) -> Self {
        Self {
            revision: 0,
            control_port: DEFAULT_CONTROL_PORT,
            tunnel_port: None,
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

    /// The UDP port of the tunnels: the configured one, or the port after
    /// the control port. A tunnel through NAT also sends from the port above
    /// it, so for the last control ports the tunnel port goes below.
    pub fn tunnel_port(&self) -> u16 {
        self.tunnel_port
            .unwrap_or(if self.control_port <= u16::MAX - 2 {
                self.control_port + 1
            } else {
                self.control_port - 2
            })
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
    /// Unique name, also used in interface names (`vx-<name>`).
    pub name: String,
    /// Address of the site in the existing transport network (e.g. its VPN
    /// address). GRETAP tunnels run between these addresses.
    pub address: Ipv4Addr,
    /// Identity of the box (its public key, see [`crate::identity`]); the
    /// keys of its tunnels derive from it. `None` for a site of an older
    /// version that has not claimed one yet: its tunnels have no keys.
    pub id: Option<SiteId>,
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

/// How a tunnel runs. Both are VXLAN on the tunnel port; they differ in who
/// may send first. Servers among each other always run direct; the tunnel
/// of a client runs direct once its server reached it directly (the daemons
/// find out by themselves, see `wanderlan_core::nat`), else through NAT.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum TunnelMode {
    /// The client may sit behind NAT: it sends from a fixed port and keeps
    /// the mapping open with keepalives; its server answers where the
    /// client's packets come from and never sends first.
    #[default]
    Nat,
    /// The client is reachable at its address: its server sends there
    /// directly, from a port per flow (spreads the load over the receive
    /// queues), without keepalives.
    Direct,
}

impl TunnelMode {
    /// The word in the status.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Nat => "nat",
            Self::Direct => "direct",
        }
    }
}

impl fmt::Display for TunnelMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for TunnelMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim() {
            "nat" => Ok(Self::Nat),
            "direct" => Ok(Self::Direct),
            other => Err(format!("\"{other}\" is no tunnel mode (nat or direct)")),
        }
    }
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
    /// Trunk ports of the default segment (`br0`), the main port first:
    /// local interfaces that carry the VLANs, bridged like the ports of a
    /// switch.
    pub trunk: Vec<String>,
    /// Trunk ports of single tunnels (peer site → interfaces). Each such list
    /// is a segment of its own at this site (own bridge); all other tunnels
    /// use `trunk`.
    pub tunnel_trunks: std::collections::BTreeMap<String, Vec<String>>,
}

impl Local {
    /// The main trunk port.
    pub fn main_trunk(&self) -> Option<&str> {
        self.trunk.first().map(String::as_str)
    }

    /// The trunk ports of the tunnel to `peer`: its own, or the default ones.
    pub fn trunk_ports_for(&self, peer: &str) -> &[String] {
        self.tunnel_trunks.get(peer).unwrap_or(&self.trunk)
    }

    /// The first trunk port of the tunnel to `peer` (it names the segment).
    pub fn trunk_for(&self, peer: &str) -> Option<&str> {
        self.trunk_ports_for(peer).first().map(String::as_str)
    }

    /// Returns true if this site has been configured (`self` is set).
    pub fn is_configured(&self) -> bool {
        self.self_name.is_some()
    }

    /// Sets the trunk ports of the default trunk (`tunnel` `None`) or of the
    /// tunnel to a site (empty: the default trunk). A port belongs to one
    /// segment: ports another segment has move here, that segment keeps the
    /// rest, and a tunnel left without ports uses the default trunk. The
    /// same list as another tunnel shares its segment. Returns what changed
    /// elsewhere, or why it cannot be (the default trunk needs a port).
    pub fn assign_trunk(
        &mut self,
        tunnel: Option<&str>,
        ports: Vec<String>,
    ) -> Result<Vec<String>, String> {
        // The same ports in another order are the same segment.
        let same =
            |a: &[String], b: &[String]| a.len() == b.len() && a.iter().all(|p| b.contains(p));
        let mut notes = Vec::new();
        let list = |p: &[String]| p.join(", ");
        match tunnel {
            None => {
                if ports.is_empty() {
                    return Err("the default trunk needs at least one port".into());
                }
                for (site, own) in &mut self.tunnel_trunks {
                    if same(own, &ports) {
                        notes.push(format!(
                            "the tunnel to {site} has the same ports and uses the default trunk then"
                        ));
                        continue;
                    }
                    let taken: Vec<String> =
                        own.iter().filter(|p| ports.contains(p)).cloned().collect();
                    if taken.is_empty() {
                        continue;
                    }
                    own.retain(|p| !ports.contains(p));
                    notes.push(if own.is_empty() {
                        format!(
                            "{} moves from the tunnel to {site}, which uses the default trunk then",
                            list(&taken)
                        )
                    } else {
                        format!(
                            "{} moves from the tunnel to {site}, which keeps {}",
                            list(&taken),
                            list(own)
                        )
                    });
                }
                self.trunk = ports;
            }
            Some(site) if ports.is_empty() || same(&ports, &self.trunk) => {
                self.tunnel_trunks.remove(site);
            }
            Some(site) => {
                // Shared with another tunnel: its list, in its order.
                let ports = self
                    .tunnel_trunks
                    .iter()
                    .find(|(other, own)| *other != site && same(own, &ports))
                    .map_or(ports, |(_, own)| own.clone());
                let taken: Vec<String> = self
                    .trunk
                    .iter()
                    .filter(|p| ports.contains(p))
                    .cloned()
                    .collect();
                if !taken.is_empty() {
                    let left: Vec<String> = self
                        .trunk
                        .iter()
                        .filter(|p| !ports.contains(p))
                        .cloned()
                        .collect();
                    if left.is_empty() {
                        return Err(format!(
                            "{} is all of the default trunk; give the default trunk another port first",
                            list(&taken)
                        ));
                    }
                    let main = if self.trunk.first() == left.first() {
                        String::new()
                    } else {
                        format!("; {} becomes its main port", left[0])
                    };
                    notes.push(format!(
                        "{} moves from the default trunk, which keeps {} for the other tunnels{main}",
                        list(&taken),
                        list(&left)
                    ));
                    self.trunk = left;
                }
                for (other, own) in &mut self.tunnel_trunks {
                    if other == site || same(own, &ports) {
                        continue;
                    }
                    let taken: Vec<String> =
                        own.iter().filter(|p| ports.contains(p)).cloned().collect();
                    if taken.is_empty() {
                        continue;
                    }
                    own.retain(|p| !ports.contains(p));
                    notes.push(if own.is_empty() {
                        format!(
                            "{} moves from the tunnel to {other}, which uses the default trunk then",
                            list(&taken)
                        )
                    } else {
                        format!(
                            "{} moves from the tunnel to {other}, which keeps {}",
                            list(&taken),
                            list(own)
                        )
                    });
                }
                self.tunnel_trunks.insert(site.to_string(), ports);
            }
        }
        // Tunnels left without ports, or with the default ones, use the
        // default trunk.
        let default = self.trunk.clone();
        self.tunnel_trunks
            .retain(|_, own| !own.is_empty() && !same(own, &default));
        Ok(notes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ports(p: &[&str]) -> Vec<String> {
        p.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn the_order_of_the_chosen_ports_does_not_split_a_segment() {
        let mut local = Local {
            trunk: ports(&["ens19", "ens20"]),
            ..Local::default()
        };
        local
            .tunnel_trunks
            .insert("lab".into(), ports(&["ens21", "ens22"]));
        // The default ports in another order: the default trunk.
        local
            .assign_trunk(Some("dc"), ports(&["ens20", "ens19"]))
            .unwrap();
        assert!(!local.tunnel_trunks.contains_key("dc"));
        // The ports of lab in another order: lab's segment, in lab's order.
        local
            .assign_trunk(Some("dc"), ports(&["ens22", "ens21"]))
            .unwrap();
        assert_eq!(local.tunnel_trunks["dc"], ports(&["ens21", "ens22"]));
        assert_eq!(local.tunnel_trunks["lab"], ports(&["ens21", "ens22"]));
        // The default trunk with lab's ports: lab merges into it, said so.
        let notes = local
            .assign_trunk(None, ports(&["ens22", "ens21"]))
            .unwrap();
        assert!(local.tunnel_trunks.is_empty(), "{:?}", local.tunnel_trunks);
        assert!(
            notes
                .iter()
                .any(|n| n.contains("the tunnel to lab has the same ports")),
            "{notes:?}"
        );
    }

    #[test]
    fn taking_the_main_port_says_which_one_follows() {
        let mut local = Local {
            trunk: ports(&["ens19", "ens20"]),
            ..Local::default()
        };
        let notes = local.assign_trunk(Some("dc"), ports(&["ens19"])).unwrap();
        assert_eq!(local.trunk, ports(&["ens20"]));
        assert!(
            notes[0].contains("ens20 becomes its main port"),
            "{notes:?}"
        );
    }

    #[test]
    fn network_lookups_and_site_vlans() {
        let mut network = Network::new(VlanSet::from_range(10, 20));
        network.sites.push(Site {
            name: "a".into(),
            address: Ipv4Addr::new(10, 0, 0, 1),
            id: None,
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
            id: None,
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
