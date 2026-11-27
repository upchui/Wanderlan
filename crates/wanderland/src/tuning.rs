//! Receive work spread over the CPUs (RPS) on the NICs wanderlan's traffic
//! comes in at: its trunk ports and the interfaces its tunnel packets
//! arrive by.
//!
//! A NIC with one receive queue (a VM without multiqueue, many small boxes)
//! does all the work of what it receives on one core: unpacking the tunnel
//! packets, the bridge, tc, nftables. Then that core limits the throughput
//! while the others are idle. RPS hands the packets to the cores by their
//! flow (for tunnel packets: by the source port, one per inner flow between
//! servers). Only where the NIC has one queue, the box more than one core
//! and nobody set RPS before; the old value comes back when wanderlan stops
//! or no longer uses the NIC.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Mutex;

use tracing::{debug, info, warn};

/// Where the NICs are.
const NET: &str = "/sys/class/net";
/// The online CPUs, e.g. `0-3,8`.
const CPUS_ONLINE: &str = "/sys/devices/system/cpu/online";
/// The CPUs kept free for other work (`isolcpus`), the same form.
const CPUS_ISOLATED: &str = "/sys/devices/system/cpu/isolated";

/// What wanderlan set, and for whom.
#[derive(Debug, Default)]
struct State {
    /// The NICs each network wants spread.
    wanted: BTreeMap<String, BTreeSet<String>>,
    /// The NICs with RPS set by wanderlan: their `rps_cpus` file and the
    /// value before.
    set: BTreeMap<String, (PathBuf, String)>,
    /// NICs looked at and left alone (several queues, set by someone else,
    /// an error): not looked at again until they drop out.
    skipped: BTreeSet<String>,
    /// `/sys` cannot be written (a container): said once.
    read_only: bool,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

/// The NICs of network `network` that should spread their receive work;
/// the union of all networks is applied.
pub fn want(network: &str, nics: BTreeSet<String>) {
    let Ok(mut guard) = STATE.lock() else {
        return;
    };
    let state = guard.get_or_insert_with(State::default);
    state.wanted.insert(network.to_string(), nics);
    let all: BTreeSet<String> = state.wanted.values().flatten().cloned().collect();
    apply(state, &all);
}

/// Puts back what wanderlan changed (when it stops).
pub fn restore_all() {
    let Ok(mut guard) = STATE.lock() else {
        return;
    };
    if let Some(state) = guard.as_mut() {
        state.wanted.clear();
        apply(state, &BTreeSet::new());
    }
}

/// The NICs wanderlan spread, by name, with the number of CPUs (for the
/// diagnosis).
pub fn spread() -> BTreeSet<String> {
    STATE
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|s| s.set.keys().cloned().collect()))
        .unwrap_or_default()
}

fn apply(state: &mut State, all: &BTreeSet<String>) {
    // Those no longer used get their old value back.
    let gone: Vec<String> = state
        .set
        .keys()
        .filter(|n| !all.contains(*n))
        .cloned()
        .collect();
    for nic in gone {
        if let Some((path, old)) = state.set.remove(&nic) {
            match std::fs::write(&path, &old) {
                Ok(()) => debug!("RPS of {nic} back to {}", old.trim()),
                Err(e) => debug!("Cannot put back RPS of {nic}: {e}"),
            }
        }
    }
    state.skipped.retain(|n| all.contains(n));
    if state.read_only {
        return;
    }
    let usable = usable_cpus();
    let cpus = usable.len();
    if cpus < 2 {
        return;
    }
    for nic in all {
        if nic == "lo" || state.set.contains_key(nic) || state.skipped.contains(nic) {
            continue;
        }
        match spread_nic(nic, &usable) {
            Ok(Some(entry)) => {
                info!(
                    "Spreading the receive work of {nic} over {cpus} CPUs (RPS): it has one receive queue"
                );
                state.set.insert(nic.clone(), entry);
            }
            Ok(None) => {
                state.skipped.insert(nic.clone());
            }
            Err(e) if e.raw_os_error() == Some(libc::EROFS) => {
                debug!("Cannot spread the receive work of {nic}: /sys is read-only");
                state.read_only = true;
                return;
            }
            Err(e) => {
                warn!("Cannot spread the receive work of {nic} over the CPUs (RPS): {e}");
                state.skipped.insert(nic.clone());
            }
        }
    }
}

/// Sets RPS of `nic` to `cpus` if it has one receive queue and no RPS:
/// the file and the value before. `None` where it is left alone.
fn spread_nic(nic: &str, cpus: &BTreeSet<usize>) -> std::io::Result<Option<(PathBuf, String)>> {
    let queues = rx_queues(nic)?;
    let [queue] = queues.as_slice() else {
        return Ok(None);
    };
    let path = queue.join("rps_cpus");
    let old = std::fs::read_to_string(&path)?;
    if !is_zero(&old) {
        return Ok(None);
    }
    std::fs::write(&path, mask(cpus))?;
    Ok(Some((path, old)))
}

/// The receive queues of `nic` (`queues/rx-*`).
pub fn rx_queues(nic: &str) -> std::io::Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(PathBuf::from(NET).join(nic).join("queues"))?
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("rx-"))
        .map(|e| e.path())
        .collect();
    out.sort();
    Ok(out)
}

/// The CPUs RPS may use: those online, without those kept free for
/// other work (`isolcpus`).
pub fn usable_cpus() -> BTreeSet<usize> {
    let read = |path: &str| std::fs::read_to_string(path).unwrap_or_default();
    let isolated = cpu_list(&read(CPUS_ISOLATED));
    cpu_list(&read(CPUS_ONLINE))
        .difference(&isolated)
        .copied()
        .collect()
}

/// The number of CPUs RPS may use.
pub fn online_cpus() -> usize {
    usable_cpus().len().max(1)
}

/// The CPUs in a list like `0-3,8`.
fn cpu_list(list: &str) -> BTreeSet<usize> {
    let mut cpus = BTreeSet::new();
    for part in list.trim().split(',').filter(|p| !p.is_empty()) {
        let (a, b) = part.split_once('-').unwrap_or((part, part));
        if let (Ok(a), Ok(b)) = (a.parse::<usize>(), b.parse::<usize>()) {
            cpus.extend(a..=b);
        }
    }
    cpus
}

/// True for a CPU mask without a CPU (`0`, `00000000,00000000`).
fn is_zero(mask: &str) -> bool {
    mask.trim().chars().filter(|c| *c != ',').all(|c| c == '0')
}

/// The RPS mask of the usable CPUs (for the diagnosis).
pub fn usable_mask() -> String {
    mask(&usable_cpus())
}

/// The mask of `cpus`, in groups of 32 bits as the kernel writes it (`fff`,
/// `ff,ffffffff`).
fn mask(cpus: &BTreeSet<usize>) -> String {
    let groups = cpus.last().map_or(1, |last| last / 32 + 1);
    let mut words = vec![0u32; groups];
    for cpu in cpus {
        words[cpu / 32] |= 1 << (cpu % 32);
    }
    let mut text = String::new();
    for (i, word) in words.iter().rev().enumerate() {
        if i == 0 {
            text.push_str(&format!("{word:x}"));
        } else {
            text.push_str(&format!(",{word:08x}"));
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_and_cpu_lists() {
        assert_eq!(mask(&cpu_list("0-3")), "f");
        assert_eq!(mask(&cpu_list("0-11")), "fff");
        assert_eq!(mask(&cpu_list("0-31")), "ffffffff");
        assert_eq!(mask(&cpu_list("0-39")), "ff,ffffffff");
        // Without the isolated ones.
        assert_eq!(mask(&cpu_list("2-3,8")), "10c");
        assert_eq!(cpu_list("0-3\n").len(), 4);
        assert_eq!(cpu_list("0-3,8,10-11").len(), 7);
        assert_eq!(cpu_list("").len(), 0);
        assert!(is_zero("0\n"));
        assert!(is_zero("00000000,00000000\n"));
        assert!(!is_zero("00000000,00000001"));
    }
}
