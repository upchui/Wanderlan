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
    /// Further trunk ports (not the first of their list): index → name.
    pub trunks: BTreeMap<u32, String>,
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
        let mut ports = ports;
        let mut map = ports.borrow_and_update().clone();
        loop {
            match sys.watch_fdb() {
                Ok(mut events) => {
                    if failing {
                        info!("Loop guard is watching the bridge again");
                        failing = false;
                    }
                    while let Some(event) = events.recv().await {
                        if !handle(&mut guards, event, &mut ports, &mut map, &inputs).await {
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
    ports: &mut watch::Receiver<PortMap>,
    map: &mut PortMap,
    inputs: &mpsc::Sender<Input>,
) -> bool {
    let entry = match event {
        FdbEvent::Learned(entry) => entry,
        // Only in the bridge it was removed from.
        FdbEvent::Removed { mac, vlan, bridge } => {
            for (b, guard) in guards.iter_mut() {
                if bridge.is_none_or(|x| x == *b) {
                    guard.forget(mac, vlan);
                }
            }
            return true;
        }
    };
    // A copy of the ports only when they changed, not for every event.
    if ports.has_changed().unwrap_or(false) {
        *map = ports.borrow_and_update().clone();
        guards.retain(|b, _| map.bridges.contains(b));
    }
    let map = &*map;
    let Some(bridge) = entry.bridge.filter(|b| map.bridges.contains(b)) else {
        return true;
    };
    if entry.permanent {
        return true;
    }
    let guard = guards.entry(bridge).or_default();
    // Moves count if a tunnel or a further trunk port takes part.
    let detected = guard.learned(Instant::now(), entry.mac, entry.vlan, entry.port, |p| {
        map.tunnels.contains_key(&p) || map.trunks.contains_key(&p)
    });
    let Some(detected) = detected else {
        return true;
    };
    // A tunnel first; between trunk ports only, the further one (never
    // the main trunk port).
    let is_tunnel = |p: u32| map.tunnels.contains_key(&p);
    let name_of = |p: u32| {
        map.tunnels
            .get(&p)
            .map(|(n, _)| n.clone())
            .or_else(|| map.trunks.get(&p).cloned())
    };
    let Some(port) = loopguard::choose_port(&detected, is_tunnel)
        .or_else(|| loopguard::choose_port(&detected, |p| map.trunks.contains_key(&p)))
        .and_then(name_of)
    else {
        return true;
    };
    let mut involved: Vec<String> = detected
        .moves
        .iter()
        .flat_map(|m| [m.from, m.to])
        .map(|p| name_of(p).unwrap_or_else(|| "the trunk".to_string()))
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

#[cfg(test)]
mod tests {
    use wanderlan_core::sys::observe::FdbEntry;

    use super::*;

    /// A MAC that keeps moving between `a` and `b` of bridge 1.
    async fn moves(map: PortMap, a: u32, b: u32) -> Option<String> {
        let (_tx, mut rx) = watch::channel(map);
        let mut map = rx.borrow_and_update().clone();
        let (inputs, mut out) = mpsc::channel(4);
        let mut guards = BTreeMap::new();
        for i in 0..30 {
            let entry = FdbEntry {
                mac: [2, 0, 0, 0, 0, 1],
                vlan: Some(20),
                port: if i % 2 == 0 { a } else { b },
                bridge: Some(1),
                permanent: false,
                updated_secs: None,
            };
            handle(
                &mut guards,
                FdbEvent::Learned(entry),
                &mut rx,
                &mut map,
                &inputs,
            )
            .await;
        }
        match out.try_recv() {
            Ok(Input::Loop(report)) => Some(report.port),
            _ => None,
        }
    }

    fn map() -> PortMap {
        PortMap {
            bridges: [1].into(),
            tunnels: [(10, ("vx-lab".to_string(), false))].into(),
            // 2 is the main trunk port ens19, 3 the further port ens20.
            trunks: [(3, "ens20".to_string())].into(),
        }
    }

    #[tokio::test]
    async fn a_loop_between_two_trunk_ports_shuts_the_further_one() {
        assert_eq!(moves(map(), 2, 3).await.as_deref(), Some("ens20"));
    }

    #[tokio::test]
    async fn a_tunnel_goes_before_a_trunk_port() {
        assert_eq!(moves(map(), 3, 10).await.as_deref(), Some("vx-lab"));
        // Main trunk and tunnel: the tunnel, as always.
        assert_eq!(moves(map(), 2, 10).await.as_deref(), Some("vx-lab"));
    }

    #[tokio::test]
    async fn without_further_ports_the_trunk_is_never_shut() {
        let mut only_main = map();
        only_main.trunks.clear();
        assert_eq!(moves(only_main, 2, 3).await, None);
    }
}
