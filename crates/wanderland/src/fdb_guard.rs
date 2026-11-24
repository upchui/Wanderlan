//! Loop guard: watches the bridge FDB and reports MAC addresses that keep
//! moving between tunnel ports.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{info, warn};
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

/// Delay before the FDB watch is set up again after it ended or failed.
const RESTART_DELAY: Duration = Duration::from_secs(1);

/// Starts watching the FDB. If the watch cannot be set up or ends, it is set
/// up again after a short delay, so the loop guard never stays silent.
pub fn spawn(
    sys: &LinuxSystem,
    ports: watch::Receiver<PortMap>,
    inputs: mpsc::Sender<Input>,
) -> JoinHandle<()> {
    let sys = sys.clone();
    tokio::spawn(async move {
        let mut guard = LoopGuard::default();
        let mut failing = false;
        loop {
            match sys.watch_fdb() {
                Ok(mut events) => {
                    if failing {
                        info!("Loop guard is watching the bridge again");
                        failing = false;
                    }
                    while let Some(event) = events.recv().await {
                        if !handle(&mut guard, event, &ports, &inputs).await {
                            return;
                        }
                    }
                    warn!("The FDB watch of the loop guard ended; restarting it");
                }
                Err(e) => {
                    if !failing {
                        warn!("Loop guard cannot watch the bridge ({e}); retrying");
                        failing = true;
                    }
                }
            }
            guard.reset();
            tokio::time::sleep(RESTART_DELAY).await;
        }
    })
}

/// Processes one FDB event. Returns false once the controller is gone.
async fn handle(
    guard: &mut LoopGuard,
    event: FdbEvent,
    ports: &watch::Receiver<PortMap>,
    inputs: &mpsc::Sender<Input>,
) -> bool {
    let entry = match event {
        FdbEvent::Learned(entry) => entry,
        FdbEvent::Removed { mac, vlan } => {
            guard.forget(mac, vlan);
            return true;
        }
    };
    let map = ports.borrow().clone();
    if entry.permanent || entry.bridge != map.bridge || map.bridge.is_none() {
        return true;
    }
    let detected = guard.learned(Instant::now(), entry.mac, entry.vlan, entry.port, |p| {
        map.tunnels.contains_key(&p)
    });
    let Some(detected) = detected else {
        return true;
    };
    let isolated = |p: u32| map.tunnels.get(&p).is_some_and(|(_, iso)| *iso);
    let Some(port) = loopguard::choose_port(&detected, isolated)
        .and_then(|p| map.tunnels.get(&p).map(|(n, _)| n.clone()))
    else {
        return true;
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
    inputs
        .send(Input::Loop(LoopReport { port, message }))
        .await
        .is_ok()
}
