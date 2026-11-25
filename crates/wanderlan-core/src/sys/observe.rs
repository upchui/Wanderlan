//! Observing the bridge: forwarding database (learned MAC addresses) and
//! the traffic counters per VLAN and port.

use std::future::Future;

use tokio::sync::mpsc;

use super::SysResult;

/// One entry of the bridge forwarding database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FdbEntry {
    /// MAC address.
    pub mac: [u8; 6],
    /// VLAN.
    pub vlan: Option<u16>,
    /// Index of the bridge port.
    pub port: u32,
    /// Index of the bridge.
    pub bridge: Option<u32>,
    /// Static or local entry (not learned).
    pub permanent: bool,
    /// Seconds since the entry was last updated.
    pub updated_secs: Option<u64>,
}

/// Traffic of one VLAN on one bridge port, counted by the bridge (needs
/// [`super::BridgeOptions::vlan_stats`]). Received: into the bridge from
/// the port; sent: out of the bridge to the port.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VlanCounters {
    /// Index of the bridge port.
    pub port: u32,
    /// VLAN.
    pub vid: u16,
    /// Bytes received.
    pub rx_bytes: u64,
    /// Packets received.
    pub rx_packets: u64,
    /// Bytes sent.
    pub tx_bytes: u64,
    /// Packets sent.
    pub tx_packets: u64,
}

/// A change of the forwarding database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FdbEvent {
    /// An address was learned or moved to another port.
    Learned(FdbEntry),
    /// An address was removed.
    Removed {
        /// MAC address.
        mac: [u8; 6],
        /// VLAN.
        vlan: Option<u16>,
        /// Index of the bridge.
        bridge: Option<u32>,
    },
}

/// Read access to the forwarding database.
pub trait ObserveOps: Send + Sync {
    /// All entries of all bridges.
    fn fdb(&self) -> impl Future<Output = SysResult<Vec<FdbEntry>>> + Send;

    /// Starts watching the forwarding database; changes arrive on the
    /// returned channel until it is dropped.
    fn watch_fdb(&self) -> SysResult<mpsc::Receiver<FdbEvent>>;

    /// The per-VLAN counters of all bridge ports.
    fn vlan_stats(&self) -> impl Future<Output = SysResult<Vec<VlanCounters>>> + Send;

    /// The index of the interface a packet from `src` to `dst` leaves by,
    /// as the kernel routes it (`ip route get <dst> from <src>`, with the
    /// policy rules); `None` if there is no route.
    fn output_interface(
        &self,
        dst: std::net::Ipv4Addr,
        src: Option<std::net::Ipv4Addr>,
    ) -> impl Future<Output = SysResult<Option<u32>>> + Send;
}
