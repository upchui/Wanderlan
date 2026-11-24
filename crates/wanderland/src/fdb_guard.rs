//! Loop guard: watches the FDB of wanderlan's bridges and reports MAC
//! addresses that keep moving between ports of one bridge (tunnels, or a
//! tunnel and the trunk).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{info, warn};
use wanderlan_core::loopguard::{self, LoopGuard};
use wanderlan_core::sys::linux::LinuxSystem;
use wanderlan_core::sys::observe::{FdbEvent, ObserveOps as _};

use crate::daemon::Input;

/// The bridges and their tunnel ports, by interface index.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PortMap {
    /// Indices of wanderlan's bridges (br0 and the bridges of further trunks).
    pub bridges: std::collections::BTreeSet<u32>,
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
        // One guard per bridge: the same MAC in two segments is no loop.
        let mut guards: BTreeMap<u32, LoopGuard> = BTreeMap::new();
        let mut failing = false;
        loop {
            match sys.watch_fdb() {
                Ok(mut events) => {
                    if failing {
                        info!("Loop guard is watching the bridge again");
                        failing = false;
                    }
                    while let Some(event) = events.recv().await {
                        if !handle(&mut guards, event, &ports, &inputs).await {
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
            guards.clear();
            tokio::time::sleep(RESTART_DELAY).await;
        }
    })
}

/// Processes one FDB event. Returns false once the controller is gone.
async fn handle(
    guards: &mut BTreeMap<u32, LoopGuard>,
    event: FdbEvent,
    ports: &watch::Receiver<PortMap>,
    inputs: &mpsc::Sender<Input>,
) -> bool {
    let entry = match event {
        FdbEvent::Learned(entry) => entry,
        FdbEvent::Removed { mac, vlan } => {
            for guard in guards.values_mut() {
                guard.forget(mac, vlan);
            }
            return true;
        }
    };
    let map = ports.borrow().clone();
    guards.retain(|b, _| map.bridges.contains(b));
    let Some(bridge) = entry.bridge.filter(|b| map.bridges.contains(b)) else {
        return true;
    };
    if entry.permanent {
        return true;
    }
    let guard = guards.entry(bridge).or_default();
    let detected = guard.learned(Instant::now(), entry.mac, entry.vlan, entry.port, |p| {
        map.tunnels.contains_key(&p)
    });
    let Some(detected) = detected else {
        return true;
    };
    let is_tunnel = |p: u32| map.tunnels.contains_key(&p);
    let Some(port) = loopguard::choose_port(&detected, is_tunnel)
        .and_then(|p| map.tunnels.get(&p).map(|(n, _)| n.clone()))
    else {
        return true;
    };
    let mut involved: Vec<String> = detected
        .moves
        .iter()
        .flat_map(|m| [m.from, m.to])
        .map(|p| {
            map.tunnels
                .get(&p)
                .map_or_else(|| "the trunk".to_string(), |(n, _)| n.clone())
        })
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
