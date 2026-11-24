//! Loop guard: watches the bridge FDB and reports MAC addresses that keep
//! moving between tunnel ports.

use std::collections::BTreeMap;
use std::time::Instant;

use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::warn;
use wanderlan_core::loopguard::{self, LoopGuard};
use wanderlan_core::sys::linux::LinuxSystem;
use wanderlan_core::sys::observe::{FdbEvent, ObserveOps as _};

use crate::daemon::Input;

/// The bridge and its tunnel ports, by interface index.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PortMap {
    /// Index of br0.
    pub bridge: Option<u32>,
    /// Tunnel ports: index → (name, isolated).
    pub tunnels: BTreeMap<u32, (String, bool)>,
}

/// A loop was detected.
#[derive(Debug)]
pub struct LoopReport {
    /// Port to shut down.
    pub port: String,
    /// Description for the log.
    pub message: String,
}

/// Starts watching the FDB.
pub fn spawn(
    sys: &LinuxSystem,
    ports: watch::Receiver<PortMap>,
    inputs: mpsc::Sender<Input>,
) -> Option<JoinHandle<()>> {
    let mut events = match sys.watch_fdb() {
        Ok(rx) => rx,
        Err(e) => {
            warn!("Loop guard disabled: {e}");
            return None;
        }
    };
    Some(tokio::spawn(async move {
        let mut guard = LoopGuard::default();
        while let Some(event) = events.recv().await {
            let FdbEvent::Learned(entry) = event else {
                if let FdbEvent::Removed { mac, vlan } = event {
                    guard.forget(mac, vlan);
                }
                continue;
            };
            let map = ports.borrow().clone();
            if entry.permanent || entry.bridge != map.bridge || map.bridge.is_none() {
                continue;
            }
            let detected = guard.learned(Instant::now(), entry.mac, entry.vlan, entry.port, |p| {
                map.tunnels.contains_key(&p)
            });
            let Some(detected) = detected else { continue };
            let isolated = |p: u32| map.tunnels.get(&p).is_some_and(|(_, iso)| *iso);
            let Some(port) = loopguard::choose_port(&detected, isolated)
                .and_then(|p| map.tunnels.get(&p).map(|(n, _)| n.clone()))
            else {
                continue;
            };
            let mut involved: Vec<String> = detected
                .moves
                .iter()
                .flat_map(|m| [m.from, m.to])
                .filter_map(|p| map.tunnels.get(&p).map(|(n, _)| n.clone()))
                .collect();
            involved.sort();
            involved.dedup();
            let vlan = detected
                .vlan
                .map_or_else(String::new, |v| format!(" in VLAN {v}"));
            let message = format!(
                "Loop detected: MAC {}{vlan} moved {} times between {} within {} s; shutting down {port}",
                loopguard::format_mac(&detected.mac),
                detected.moves.len(),
                involved.join(" and "),
                loopguard::WINDOW.as_secs()
            );
            if inputs
                .send(Input::Loop(LoopReport { port, message }))
                .await
                .is_err()
            {
                break;
            }
        }
    }))
}
