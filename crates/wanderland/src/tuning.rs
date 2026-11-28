//! Receive work spread over the CPUs (RPS), as the site's settings say:
//! `rps_transport` for the interfaces the tunnel packets come in by,
//! `rps_trunk` for the trunk ports (`off`, `on`, and for the transport
//! `auto`).
//!
//! A NIC with one receive queue (a VM without multiqueue, many small boxes)
//! does all the work of what it receives on one core: unpacking the tunnel
//! packets, the bridge, tc, nftables. RPS hands the packets to the other
//! cores by their flow. On bare metal that can double the throughput; in a
//! VM every hand-off is an expensive interrupt (with two cores it cost a
//! third). `auto` measures it (see `speedtest::calibrate`): both settings
//! with each peer, through its tunnel. A NIC carries the tunnels to several
//! peers and RPS is one setting of the NIC: each tunnel's measurement is
//! kept, and the NIC gets what all of them together speak for ([`choose`]:
//! on only if it brings clearly more in total and no tunnel clearly less);
//! a tunnel that is gone drops out. Only NICs with one queue, on a box with
//! more than one core, where nobody set RPS before; the old value comes
//! back when wanderlan stops, resets, or the setting goes back to `off`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tracing::{debug, info};
use wanderlan_core::config::RpsMode;

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
    /// What the settings say for it.
    mode: RpsMode,
    /// A setting for a measurement, until then (`auto`).
    lease: Option<(bool, Instant)>,
    /// What each tunnel received through it with RPS off and on (bit/s),
    /// and when, by peer (`auto`).
    tunnels: BTreeMap<String, (u64, u64, Instant)>,
}

impl Nic {
    /// RPS on now: as the setting says, as measured, or for a measurement.
    fn on(&self) -> bool {
        match (self.lease, self.mode) {
            (Some((on, _)), RpsMode::Auto) => on,
            (_, RpsMode::On) => true,
            (_, RpsMode::Off) => false,
            (_, RpsMode::Auto) => decision(self).on,
        }
    }

    /// Writes what [`Nic::on`] says.
    fn apply(&self, name: &str) {
        let value = if self.on() {
            usable_mask()
        } else {
            self.original.clone()
        };
        if let Err(e) = write(&self.path, &value) {
            debug!("Cannot set RPS of {name}: {e}");
        }
    }
}

/// RPS stays on only with at least this share (percent) of what off got,
/// over all tunnels of a NIC: clearly more, not noise.
pub const CLEARLY_MORE: u64 = 110;
/// ... and no tunnel may get less than this share (percent).
const AT_LEAST: u64 = 90;

/// The setting of a NIC from what its tunnels received with RPS off and on
/// (bit/s): on only if clearly more in total, and no tunnel clearly less.
pub fn choose(tunnels: &[(u64, u64)]) -> bool {
    let off: u128 = tunnels.iter().map(|(off, _)| u128::from(*off)).sum();
    let on: u128 = tunnels.iter().map(|(_, on)| u128::from(*on)).sum();
    on > 0
        && on * 100 >= off * u128::from(CLEARLY_MORE)
        && tunnels
            .iter()
            .all(|(off, on)| u128::from(*on) * 100 >= u128::from(*off) * u128::from(AT_LEAST))
}

/// What a NIC keeps, from the measurements of its tunnels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    /// RPS on.
    pub on: bool,
    /// The tunnels measured on it.
    pub tunnels: usize,
    /// What they received with RPS off, together (bit/s).
    pub off_bps: u64,
    /// ... and with RPS on.
    pub on_bps: u64,
}

fn decision(entry: &Nic) -> Decision {
    let measured: Vec<(u64, u64)> = entry.tunnels.values().map(|(o, n, _)| (*o, *n)).collect();
    Decision {
        on: choose(&measured),
        tunnels: measured.len(),
        off_bps: measured.iter().map(|(o, _)| *o).sum(),
        on_bps: measured.iter().map(|(_, n)| *n).sum(),
    }
}

/// The NICs wanderlan looks after, and what each network of the box wants
/// for which NIC.
#[derive(Debug, Default)]
struct State {
    nics: BTreeMap<String, Nic>,
    wanted: BTreeMap<String, BTreeMap<String, RpsMode>>,
    /// NICs that cannot be changed (several queues, set by someone else,
    /// `/sys` read-only), with why: said once.
    cannot: BTreeMap<String, String>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn with<T>(f: impl FnOnce(&mut State) -> T) -> Option<T> {
    let mut guard = STATE.lock().ok()?;
    Some(f(guard.get_or_insert_with(State::default)))
}

/// What network `network` wants: `mode` for each of `nics` (its transport
/// interfaces with `rps_transport`, its trunk ports with `rps_trunk`).
/// Several networks on one NIC: `auto` before `on` before `off`.
pub fn want(network: &str, nics: BTreeMap<String, RpsMode>) {
    with(|state| {
        state.wanted.insert(network.to_string(), nics);
        let mut modes: BTreeMap<String, RpsMode> = BTreeMap::new();
        for wanted in state.wanted.values() {
            for (nic, mode) in wanted {
                let rank = |m: RpsMode| match m {
                    RpsMode::Off => 0,
                    RpsMode::On => 1,
                    RpsMode::Auto => 2,
                };
                let entry = modes.entry(nic.clone()).or_insert(*mode);
                if rank(*mode) > rank(*entry) {
                    *entry = *mode;
                }
            }
        }
        // Back to what it was where nothing wants RPS any more.
        let gone: Vec<String> = state
            .nics
            .keys()
            .filter(|n| modes.get(*n).is_none_or(|m| *m == RpsMode::Off))
            .cloned()
            .collect();
        for name in gone {
            if let Some(nic) = state.nics.remove(&name) {
                if let Err(e) = write(&nic.path, &nic.original) {
                    debug!("Cannot put back RPS of {name}: {e}");
                }
                info!("RPS of {name} back to how it was");
            }
        }
        for (name, mode) in modes {
            if mode == RpsMode::Off {
                continue;
            }
            if let Some(nic) = state.nics.get_mut(&name) {
                if nic.mode != mode {
                    nic.mode = mode;
                    nic.apply(&name);
                }
                continue;
            }
            match prepare(&name) {
                Ok(mut nic) => {
                    nic.mode = mode;
                    nic.apply(&name);
                    if mode == RpsMode::On {
                        info!(
                            "Spreading the receive work of {name} over {} cores (RPS, as set)",
                            online_cpus()
                        );
                    }
                    state.cannot.remove(&name);
                    state.nics.insert(name, nic);
                }
                Err(why) => {
                    if state.cannot.get(&name) != Some(&why) {
                        info!("RPS of {name} stays as it is: {why}");
                        state.cannot.insert(name, why);
                    }
                }
            }
        }
    });
}

/// The calibration of a NIC as the diagnosis shows it.
#[derive(Debug, Clone)]
pub struct Calibrated {
    /// What it keeps.
    pub decision: Decision,
    /// How long ago the last of its tunnels was measured.
    pub ago: Duration,
}

/// Why RPS of `nic` cannot be measured (switched for a measurement), if it
/// cannot: the setting is not `auto`, several receive queues, one core, set
/// by someone else, `/sys` read-only.
pub fn cannot(nic: &str) -> Option<String> {
    with(|state| match state.nics.get(nic) {
        Some(n) if n.mode == RpsMode::Auto => None,
        Some(n) => Some(format!(
            "RPS of {nic} is fixed by the setting ({})",
            n.mode.as_str()
        )),
        None => Some(state.cannot.get(nic).cloned().unwrap_or_else(|| {
            format!("RPS of {nic} is fixed by the setting (off; auto measures)")
        })),
    })
    .unwrap_or_else(|| Some("internal error".into()))
}

/// The mode the settings give `nic` (`off` if none).
pub fn mode(nic: &str) -> RpsMode {
    with(|state| state.nics.get(nic).map_or(RpsMode::Off, |n| n.mode)).unwrap_or_default()
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
        mode: RpsMode::Off,
        lease: None,
        tunnels: BTreeMap::new(),
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

/// Sets RPS of `nic` for a measurement, until `lease` ends (then back to
/// what was decided). Only with `auto`.
pub fn try_setting(nic: &str, on: bool, lease: Duration) -> Result<(), String> {
    if let Some(why) = cannot(nic) {
        return Err(why);
    }
    with(|state| {
        let entry = state.nics.get_mut(nic).ok_or("not looked after")?;
        entry.lease = Some((on, Instant::now() + lease));
        let value = if on {
            usable_mask()
        } else {
            entry.original.clone()
        };
        write(&entry.path, &value)
    })
    .unwrap_or_else(|| Err("internal error".into()))
}

/// What the tunnel to `peer` received through `nic` with RPS off and on:
/// kept with the others of the NIC, which gets the setting all of them
/// speak for (the measurement's own setting ends). Only with `auto`.
pub fn record(nic: &str, peer: &str, off_bps: u64, on_bps: u64) -> Result<Decision, String> {
    if let Some(why) = cannot(nic) {
        return Err(why);
    }
    with(|state| {
        let entry = state.nics.get_mut(nic).ok_or("not looked after")?;
        entry
            .tunnels
            .insert(peer.to_string(), (off_bps, on_bps, Instant::now()));
        entry.lease = None;
        entry.apply(nic);
        Ok(decision(entry))
    })
    .unwrap_or_else(|| Err("internal error".into()))
}

/// The tunnels to `peers` are gone: their measurements no longer count;
/// their NICs get the setting the others speak for (off without any).
pub fn forget(peers: &[String]) {
    with(|state| {
        for (name, nic) in &mut state.nics {
            let before = nic.tunnels.len();
            nic.tunnels.retain(|peer, _| !peers.contains(peer));
            if nic.tunnels.len() != before && nic.lease.is_none() {
                nic.apply(name);
            }
        }
    });
}

/// Ends the settings for measurements whose time is up (every second).
pub fn expire_leases() {
    with(|state| {
        let now = Instant::now();
        for (name, nic) in &mut state.nics {
            if nic.lease.is_some_and(|(_, until)| until <= now) {
                nic.lease = None;
                nic.apply(name);
            }
        }
    });
}

/// The calibration of `nic`, for the diagnosis.
pub fn calibrated(nic: &str) -> Option<Calibrated> {
    with(|state| {
        let entry = state.nics.get(nic)?;
        let last = entry.tunnels.values().map(|(_, _, at)| *at).max()?;
        Some(Calibrated {
            decision: decision(entry),
            ago: last.elapsed(),
        })
    })
    .flatten()
}

/// Puts back what wanderlan changed (when it stops or resets).
pub fn restore_all() {
    with(|state| {
        state.wanted.clear();
        state.cannot.clear();
        for (name, nic) in std::mem::take(&mut state.nics) {
            match write(&nic.path, &nic.original) {
                Ok(()) => debug!("RPS of {name} back to {}", nic.original.trim()),
                Err(e) => debug!("Cannot put back RPS of {name}: {e}"),
            }
        }
    });
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

    #[test]
    fn a_nic_keeps_what_all_its_tunnels_speak_for() {
        // Clearly more in total, none clearly less: on.
        assert!(choose(&[(1_000, 1_200), (1_000, 1_050)]));
        // Within the noise in total: off.
        assert!(!choose(&[(1_000, 1_050), (1_000, 1_080)]));
        // More in total, but one tunnel clearly less: off.
        assert!(!choose(&[(1_000, 2_000), (1_000, 800)]));
        // Nothing measured, nothing arrived: off.
        assert!(!choose(&[]));
        assert!(!choose(&[(0, 0)]));
        // One tunnel alone: 10 % more.
        assert!(choose(&[(1_000, 1_100)]));
        assert!(!choose(&[(1_400, 900)]));
    }
}
