//! Access to the kernel, abstracted behind traits.
//!
//! [`NetOps`] covers links, the bridge, bridge port settings and bridge VLANs.
//! [`netlink::Netlink`] implements it with rtnetlink, [`fake::FakeNet`] with an
//! in-memory model of the kernel so that the reconciliation logic can be
//! tested without root privileges.

pub mod fake;
pub mod linux;
pub mod netlink;
mod netlink_observe;
mod netlink_tc;
pub mod nft;
pub mod nft_ip;
pub mod observe;
pub mod tc;

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::net::{Ipv4Addr, SocketAddrV4};

/// Error from a system call, with a description of what was attempted.
#[derive(Debug, thiserror::Error)]
#[error("{op}: {source}")]
pub struct SysError {
    /// What was attempted, e.g. "create VXLAN interface vx-heim".
    pub op: String,
    /// The underlying error.
    #[source]
    pub source: Box<dyn std::error::Error + Send + Sync>,
}

impl SysError {
    /// Wraps an error with a description of the operation.
    pub fn new(
        op: impl Into<String>,
        source: impl Into<Box<dyn std::error::Error + Send + Sync>>,
    ) -> Self {
        Self {
            op: op.into(),
            source: source.into(),
        }
    }
}

/// Result type of system operations.
pub type SysResult<T> = Result<T, SysError>;

/// Bridge options managed by wanderlan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BridgeOptions {
    /// `vlan_filtering`.
    pub vlan_filtering: bool,
    /// Kernel STP on or off.
    pub stp: bool,
    /// `vlan_default_pvid`; 0 means ports get no VLAN automatically.
    pub default_pvid: u16,
    /// Whether bridged traffic passes iptables/ip6tables/arptables
    /// (only relevant if `br_netfilter` is loaded).
    pub nf_call: bool,
    /// Traffic counters per VLAN and port (`vlan_stats_enabled` and
    /// `vlan_stats_per_port`). The kernel sets the per-port part only while
    /// no port has VLANs, so in practice only when the bridge is created.
    pub vlan_stats: bool,
}

/// Parameters of a VXLAN tunnel interface (one per peer site). Every
/// tunnel sends from any address of this box (the kernel picks it per
/// packet); where it sends to is its default FDB entry, see
/// [`NetOps::set_tunnel_remote`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VxlanParams {
    /// VXLAN network identifier (24 bits), derived from the identities of
    /// both sites: the kernel drops frames of a box with another one.
    pub vni: u32,
    /// The UDP port the tunnel receives on (and sends to by default).
    pub port: u16,
    /// A fixed source port, so that a NAT keeps one mapping for the tunnel;
    /// `None`: one per flow (spreads the load over the receive queues).
    pub source_port: Option<u16>,
}

/// Where the VXLAN interfaces send to, by index: each address with its
/// port, `None` where it is the interface's own (the kernel leaves it out
/// then).
pub type TunnelRemotes = BTreeMap<u32, Vec<(Ipv4Addr, Option<u16>)>>;

/// Kind of a link as far as wanderlan cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkKind {
    /// A bridge with its options.
    Bridge(BridgeOptions),
    /// A VXLAN tunnel with its parameters.
    Vxlan(VxlanParams),
    /// Anything else; the string is the kernel's kind ("" for physical).
    Other(String),
}

/// Observed state of one network interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkState {
    /// Interface index.
    pub index: u32,
    /// Interface name.
    pub name: String,
    /// Kind and kind-specific parameters.
    pub kind: LinkKind,
    /// Interface alias; wanderlan marks its interfaces with [`crate::desired::OWNER_ALIAS`].
    pub alias: Option<String>,
    /// Index of the bridge (or other master) this link is attached to.
    pub master: Option<u32>,
    /// Administratively up.
    pub admin_up: bool,
    /// Operationally up (carrier).
    pub oper_up: bool,
    /// MTU.
    pub mtu: u32,
    /// Bridge port isolation, if the link is a bridge port.
    pub isolated: Option<bool>,
    /// Counters (bytes, packets, errors, drops).
    pub traffic: crate::api::Traffic,
}

/// Flags of a bridge VLAN entry on a port.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VlanFlags {
    /// Untagged frames received on the port belong to this VLAN.
    pub pvid: bool,
    /// Frames of this VLAN leave the port untagged.
    pub untagged: bool,
}

impl VlanFlags {
    /// A tagged member.
    pub const TAGGED: Self = Self {
        pvid: false,
        untagged: false,
    };
    /// The native (untagged) VLAN of a port.
    pub const NATIVE: Self = Self {
        pvid: true,
        untagged: true,
    };
}

impl fmt::Display for VlanFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.pvid, self.untagged) {
            (false, false) => f.write_str("tagged"),
            (true, true) => f.write_str("pvid untagged"),
            (true, false) => f.write_str("pvid"),
            (false, true) => f.write_str("untagged"),
        }
    }
}

/// VLAN entries of one bridge port, by VLAN ID.
pub type PortVlans = BTreeMap<u16, VlanFlags>;

/// Changes to the basic attributes of a link. `None` leaves a value as is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkUpdate {
    /// Bring the link up or down.
    pub up: Option<bool>,
    /// Attach to a master (Some(Some)) or detach (Some(None)).
    pub master: Option<Option<u32>>,
    /// New MTU.
    pub mtu: Option<u32>,
    /// New alias.
    pub alias: Option<String>,
}

/// Network configuration operations (links, bridge, ports, VLANs).
///
/// All methods identify links by index; [`LinkState`] snapshots from
/// [`NetOps::links`] provide the mapping from names.
pub trait NetOps: Send + Sync {
    /// Lists all links.
    fn links(&self) -> impl Future<Output = SysResult<Vec<LinkState>>> + Send;

    /// Lists the VLAN entries of all bridge ports (and bridges), by index.
    fn port_vlans(&self) -> impl Future<Output = SysResult<BTreeMap<u32, PortVlans>>> + Send;

    /// Creates a bridge.
    fn create_bridge(
        &self,
        name: &str,
        options: &BridgeOptions,
    ) -> impl Future<Output = SysResult<()>> + Send;

    /// False once the kernel turned out to have no per-VLAN counters per
    /// port (a bridge created with them came back without); bridges are
    /// then not rebuilt for them.
    fn vlan_stats_supported(&self) -> bool {
        true
    }

    /// Changes the options of an existing bridge (all but the per-port part
    /// of [`BridgeOptions::vlan_stats`], which only a new bridge gets).
    fn set_bridge_options(
        &self,
        index: u32,
        name: &str,
        options: &BridgeOptions,
    ) -> impl Future<Output = SysResult<()>> + Send;

    /// Creates a VXLAN interface (without learning, UDP checksum or DF on
    /// the outer header).
    fn create_vxlan(
        &self,
        name: &str,
        params: &VxlanParams,
        mtu: u32,
    ) -> impl Future<Output = SysResult<()>> + Send;

    /// Where the VXLAN interface `index` sends to: the first address of
    /// its default FDB entry with its port (`None` where it is the
    /// interface's own: the kernel leaves it out then), `None` without one.
    /// One small request per interface: the forwarding table of the bridge
    /// (every MAC address learned) is not read.
    fn tunnel_remote(
        &self,
        index: u32,
    ) -> impl Future<Output = SysResult<Option<(Ipv4Addr, Option<u16>)>>> + Send;

    /// Sets or removes where a VXLAN interface sends to, in place: the
    /// interface and its bridge port stay. `own_port` is the interface's
    /// port (that of the entries listed without one).
    fn set_tunnel_remote(
        &self,
        index: u32,
        name: &str,
        own_port: u16,
        remote: Option<SocketAddrV4>,
    ) -> impl Future<Output = SysResult<()>> + Send;

    /// Deletes a link.
    fn delete_link(&self, index: u32) -> impl Future<Output = SysResult<()>> + Send;

    /// Changes basic link attributes.
    fn update_link(
        &self,
        index: u32,
        update: &LinkUpdate,
    ) -> impl Future<Output = SysResult<()>> + Send;

    /// Sets bridge port isolation.
    fn set_port_isolated(
        &self,
        index: u32,
        isolated: bool,
    ) -> impl Future<Output = SysResult<()>> + Send;

    /// Adds or updates VLAN entries. `bridge_self` targets the bridge device
    /// itself instead of a port.
    fn add_vlans(
        &self,
        index: u32,
        bridge_self: bool,
        vlans: &[(u16, VlanFlags)],
    ) -> impl Future<Output = SysResult<()>> + Send;

    /// Removes all dynamically learned MAC addresses of a bridge.
    fn flush_fdb(&self, bridge: u32, name: &str) -> impl Future<Output = SysResult<()>> + Send;

    /// Removes VLAN entries.
    fn del_vlans(
        &self,
        index: u32,
        bridge_self: bool,
        vids: &[u16],
    ) -> impl Future<Output = SysResult<()>> + Send;
}

/// Everything the daemon needs from the system.
pub trait System:
    NetOps + tc::TcOps + nft::NftOps + observe::ObserveOps + nft_ip::LearnOps
{
}

impl<T> System for T where
    T: NetOps + tc::TcOps + nft::NftOps + observe::ObserveOps + nft_ip::LearnOps
{
}

/// Groups VLAN entries into ranges of consecutive IDs with equal flags.
/// Entries with the PVID flag are never part of a range (the kernel rejects
/// PVID ranges).
pub fn vlan_ranges(vlans: &[(u16, VlanFlags)]) -> Vec<(u16, u16, VlanFlags)> {
    let mut sorted = vlans.to_vec();
    sorted.sort_unstable();
    let mut out: Vec<(u16, u16, VlanFlags)> = Vec::new();
    for (vid, flags) in sorted {
        match out.last_mut() {
            Some((_, last, f))
                if *f == flags && !flags.pvid && last.checked_add(1) == Some(vid) =>
            {
                *last = vid;
            }
            _ => out.push((vid, vid, flags)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_respect_flags_and_pvid() {
        let t = VlanFlags::TAGGED;
        let n = VlanFlags::NATIVE;
        let vlans = vec![
            (12, t),
            (10, t),
            (11, t),
            (20, n),
            (21, t),
            (22, t),
            (30, t),
        ];
        assert_eq!(
            vlan_ranges(&vlans),
            vec![(10, 12, t), (20, 20, n), (21, 22, t), (30, 30, t)]
        );
    }
}
