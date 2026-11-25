//! Throughput of the interfaces (tunnels and trunk), sampled from the
//! interface counters, and of every VLAN on every bridge port, sampled from
//! the bridge's per-VLAN counters.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use tokio::task::JoinHandle;
use wanderlan_core::sys::NetOps as _;
use wanderlan_core::sys::linux::LinuxSystem;
use wanderlan_core::sys::observe::{ObserveOps as _, VlanCounters};

/// Sampling interval.
const INTERVAL: Duration = Duration::from_secs(2);

/// Receive and transmit rate in bit/s per interface.
pub type Rates = Arc<RwLock<BTreeMap<String, (u64, u64)>>>;

/// Per port name and VLAN: receive and transmit rate in bit/s and the
/// counters.
pub type VlanRates = Arc<RwLock<BTreeMap<(String, u16), VlanRate>>>;

/// The traffic of one VLAN on one port.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VlanRate {
    /// Receive rate in bit/s.
    pub rx_bps: u64,
    /// Transmit rate in bit/s.
    pub tx_bps: u64,
    /// The counters.
    pub counters: VlanCounters,
}

/// The VLAN counters of one sample, by port index and VLAN.
type VlanSample = BTreeMap<(u32, u16), VlanCounters>;

/// Bit/s from two byte counters `secs` apart (0 if the counter went back).
fn rate(new: u64, old: u64, secs: f64) -> u64 {
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let bps = (new.saturating_sub(old) as f64 * 8.0 / secs.max(0.001)) as u64;
    bps
}

/// The VLAN rates from two samples `secs` apart; ports are named by
/// `name_of` (index → name), unknown ones are left out.
fn vlan_rates(
    now: &[VlanCounters],
    before: &VlanSample,
    secs: f64,
    name_of: impl Fn(u32) -> Option<String>,
) -> BTreeMap<(String, u16), VlanRate> {
    now.iter()
        .filter_map(|c| {
            let name = name_of(c.port)?;
            let (rx_bps, tx_bps) = before.get(&(c.port, c.vid)).map_or((0, 0), |b| {
                (
                    rate(c.rx_bytes, b.rx_bytes, secs),
                    rate(c.tx_bytes, b.tx_bytes, secs),
                )
            });
            Some((
                (name, c.vid),
                VlanRate {
                    rx_bps,
                    tx_bps,
                    counters: *c,
                },
            ))
        })
        .collect()
}

/// Starts the sampler.
pub fn spawn(sys: LinuxSystem, rates: Rates, vlans: VlanRates) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut last: BTreeMap<String, (Instant, u64, u64)> = BTreeMap::new();
        let mut last_vlans: Option<(Instant, VlanSample)> = None;
        let mut ticker = tokio::time::interval(INTERVAL);
        loop {
            ticker.tick().await;
            let Ok(links) = sys.links().await else {
                continue;
            };
            let now = Instant::now();
            let mut out = BTreeMap::new();
            let mut next = BTreeMap::new();
            for link in &links {
                let (rx_bytes, tx_bytes) = (link.traffic.rx_bytes, link.traffic.tx_bytes);
                if let Some((then, rx, tx)) = last.get(&link.name) {
                    let secs = now.duration_since(*then).as_secs_f64();
                    out.insert(
                        link.name.clone(),
                        (rate(rx_bytes, *rx, secs), rate(tx_bytes, *tx, secs)),
                    );
                }
                next.insert(link.name.clone(), (now, rx_bytes, tx_bytes));
            }
            last = next;
            if let Ok(mut r) = rates.write() {
                *r = out;
            }
            // Only ports of a bridge have per-VLAN counters.
            let counters = match sys.vlan_stats().await {
                Ok(c) => c,
                Err(e) => {
                    tracing::debug!("{e}");
                    Vec::new()
                }
            };
            let secs = last_vlans
                .as_ref()
                .map_or(0.0, |(then, _)| now.duration_since(*then).as_secs_f64());
            let before = last_vlans.map(|(_, b)| b).unwrap_or_default();
            let name_of = |index: u32| {
                links
                    .iter()
                    .find(|l| l.index == index)
                    .map(|l| l.name.clone())
            };
            let out = vlan_rates(&counters, &before, secs, name_of);
            last_vlans = Some((
                now,
                counters.iter().map(|c| ((c.port, c.vid), *c)).collect(),
            ));
            if let Ok(mut v) = vlans.write() {
                *v = out;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vlan_rates_from_two_samples() {
        let c = |vid, rx_bytes, tx_bytes| VlanCounters {
            port: 3,
            vid,
            rx_bytes,
            tx_bytes,
            ..VlanCounters::default()
        };
        let before = BTreeMap::from([((3, 20), c(20, 1000, 0))]);
        let now = [
            c(20, 3000, 500),
            c(30, 100, 0),
            VlanCounters {
                port: 9,
                ..c(20, 1, 1)
            },
        ];
        let name_of = |i: u32| (i == 3).then(|| "gt-buero".to_string());
        let got = vlan_rates(&now, &before, 2.0, name_of);
        assert_eq!(got.len(), 2, "port 9 is unknown");
        let r = got[&("gt-buero".to_string(), 20)];
        assert_eq!((r.rx_bps, r.tx_bps), (8000, 2000));
        assert_eq!(r.counters.rx_bytes, 3000);
        // A VLAN seen the first time has no rate yet.
        let r = got[&("gt-buero".to_string(), 30)];
        assert_eq!((r.rx_bps, r.tx_bps), (0, 0));
    }
}
