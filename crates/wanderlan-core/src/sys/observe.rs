//! Observing the bridge: forwarding database (learned MAC addresses).

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
    },
}

/// Read access to the forwarding database.
pub trait ObserveOps: Send + Sync {
    /// All entries of all bridges.
    fn fdb(&self) -> impl Future<Output = SysResult<Vec<FdbEntry>>> + Send;

    /// Starts watching the forwarding database; changes arrive on the
    /// returned channel until it is dropped.
    fn watch_fdb(&self) -> SysResult<mpsc::UnboundedReceiver<FdbEvent>>;
}
