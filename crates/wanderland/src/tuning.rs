//! Receive work spread over the CPUs (RPS) on the NIC the tunnel packets
//! come in by, where a measurement showed that it helps.
//!
//! A NIC with one receive queue (a VM without multiqueue, many small boxes)
//! does all the work of what it receives on one core: unpacking the tunnel
//! packets, the bridge, tc, nftables. RPS hands the packets to the other
//! cores by their flow. On bare metal that can double the throughput; in a
//! VM every hand-off is an expensive interrupt, and with two cores it cost a
//! third. So it is measured (see `speedtest::calibrate`): both settings with
//! the peer, the better one stays. Without a measurement RPS stays off; only
//! where the NIC has one queue, the box more than one core and nobody set
//! RPS before. The old value comes back when wanderlan stops.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tracing::debug;

/// Where the NICs are.
const NET: &str = "/sys/class/net";
/// The online CPUs, e.g. `0-3,8`.
const CPUS_ONLINE: &str = "/sys/devices/system/cpu/online";
/// The CPUs kept free for other work (`isolcpus`), the same form.
const CPUS_ISOLATED: &str = "/sys/devices/system/cpu/isolated";

/// What wanderlan knows about one NIC whose RPS it may set.
#[derive(Debug, Clone)]
struct Nic {
    /// The `rps_cpus` file of its one receive queue.
    path: PathBuf,
    /// Its value before wanderlan touched it (no CPU).
    original: String,
    /// The setting the last measurement chose (none: off).
    decided: Option<bool>,
    /// A setting for a measurement, until then.
    lease: Option<(bool, Instant)>,
    /// When it was measured last, and what came out (for the diagnosis).
    calibrated: Option<(Instant, String)>,
}

static STATE: Mutex<BTreeMap<String, Nic>> = Mutex::new(BTreeMap::new());

/// The calibration of a NIC as the diagnosis shows it.
#[derive(Debug, Clone)]
pub struct Calibrated {
    /// RPS on.
    pub on: bool,
    /// What was measured.
    pub summary: String,
    /// How long ago.
    pub ago: Duration,
}

/// Why RPS of `nic` cannot be set by wanderlan, if it cannot: several
/// receive queues (no need), one core, set by someone else, `/sys`
/// read-only.
pub fn cannot(nic: &str) -> Option<String> {
    if STATE.lock().is_ok_and(|s| s.contains_key(nic)) {
        return None;
    }
    prepare(nic).err()
}

/// The NIC as wanderlan may change it, or why not.
fn prepare(nic: &str) -> Result<Nic, String> {
    if online_cpus() < 2 {
        return Err("the box has one core".into());
    }
    let queues = rx_queues(nic).map_err(|e| format!("{nic}: {e}"))?;
    let [queue] = queues.as_slice() else {
        return Err(format!(
            "{nic} has {} receive queues (no need)",
            queues.len()
        ));
    };
    let path = queue.join("rps_cpus");
    let original = std::fs::read_to_string(&path).map_err(|e| format!("{nic}: {e}"))?;
    if !is_zero(&original) {
        return Err(format!(
            "RPS of {nic} was set by someone else ({})",
            original.trim()
        ));
    }
    // Writing the same value back tells whether it can be written at all.
    write(&path, &original)?;
    Ok(Nic {
        path,
        original,
        decided: None,
        lease: None,
        calibrated: None,
    })
}

fn write(path: &PathBuf, value: &str) -> Result<(), String> {
    std::fs::write(path, value).map_err(|e| {
        if e.raw_os_error() == Some(libc::EROFS) {
            "/sys cannot be written here (a container?)".to_string()
        } else {
            format!("{}: {e}", path.display())
        }
    })
}

/// The value of the file for `on`.
fn value(nic: &Nic, on: bool) -> String {
    if on {
        usable_mask()
    } else {
        nic.original.clone()
    }
}

/// Sets RPS of `nic` for a measurement, until `lease` ends (then back to
/// what was decided).
pub fn try_setting(nic: &str, on: bool, lease: Duration) -> Result<(), String> {
    let mut state = STATE.lock().map_err(|_| "internal error".to_string())?;
    let entry = match state.get(nic) {
        Some(entry) => entry.clone(),
        None => prepare(nic)?,
    };
    write(&entry.path, &value(&entry, on))?;
    let entry = state.entry(nic.to_string()).or_insert(entry);
    entry.lease = Some((on, Instant::now() + lease));
    Ok(())
}

/// Keeps RPS of `nic` on or off, as measured (`summary`).
pub fn decide(nic: &str, on: bool, summary: String) -> Result<(), String> {
    let mut state = STATE.lock().map_err(|_| "internal error".to_string())?;
    let entry = match state.get(nic) {
        Some(entry) => entry.clone(),
        None => prepare(nic)?,
    };
    write(&entry.path, &value(&entry, on))?;
    let entry = state.entry(nic.to_string()).or_insert(entry);
    entry.lease = None;
    entry.decided = Some(on);
    entry.calibrated = Some((Instant::now(), summary));
    Ok(())
}

/// Ends the settings for measurements whose time is up (every second).
pub fn expire_leases() {
    let Ok(mut state) = STATE.lock() else {
        return;
    };
    let now = Instant::now();
    for (nic, entry) in state.iter_mut() {
        if entry.lease.is_some_and(|(_, until)| until <= now) {
            entry.lease = None;
            let back = value(entry, entry.decided.unwrap_or(false));
            if let Err(e) = write(&entry.path, &back) {
                debug!("Cannot put back RPS of {nic}: {e}");
            }
        }
    }
}

/// The calibration of `nic`, for the diagnosis.
pub fn calibrated(nic: &str) -> Option<Calibrated> {
    let state = STATE.lock().ok()?;
    let entry = state.get(nic)?;
    let (at, summary) = entry.calibrated.as_ref()?;
    Some(Calibrated {
        on: entry.decided.unwrap_or(false),
        summary: summary.clone(),
        ago: at.elapsed(),
    })
}

/// Puts back what wanderlan changed (when it stops).
pub fn restore_all() {
    let Ok(mut state) = STATE.lock() else {
        return;
    };
    for (nic, entry) in std::mem::take(&mut *state) {
        match write(&entry.path, &entry.original) {
            Ok(()) => debug!("RPS of {nic} back to {}", entry.original.trim()),
            Err(e) => debug!("Cannot put back RPS of {nic}: {e}"),
        }
    }
}

/// True if RPS of `nic` is set (by anyone).
pub fn rps_set(nic: &str) -> bool {
    std::fs::read_to_string(PathBuf::from(NET).join(nic).join("queues/rx-0/rps_cpus"))
        .is_ok_and(|v| !is_zero(&v))
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
