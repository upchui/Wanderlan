//! Throughput of the tunnel ports, sampled from the interface counters.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use tokio::task::JoinHandle;
use wanderlan_core::sys::NetOps as _;
use wanderlan_core::sys::linux::LinuxSystem;
use wanderlan_core::topology::GRETAP_PREFIX;

/// Sampling interval.
const INTERVAL: Duration = Duration::from_secs(5);

/// Receive and transmit rate in bit/s per interface.
pub type Rates = Arc<RwLock<BTreeMap<String, (u64, u64)>>>;

/// Starts the sampler.
pub fn spawn(sys: LinuxSystem, rates: Rates) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut last: BTreeMap<String, (Instant, u64, u64)> = BTreeMap::new();
        let mut ticker = tokio::time::interval(INTERVAL);
        loop {
            ticker.tick().await;
            let Ok(links) = sys.links().await else {
                continue;
            };
            let now = Instant::now();
            let mut out = BTreeMap::new();
            let mut next = BTreeMap::new();
            for link in links.iter().filter(|l| l.name.starts_with(GRETAP_PREFIX)) {
                if let Some((then, rx, tx)) = last.get(&link.name) {
                    let secs = now.duration_since(*then).as_secs_f64().max(0.001);
                    let rate = |new: u64, old: u64| {
                        #[allow(
                            clippy::cast_precision_loss,
                            clippy::cast_possible_truncation,
                            clippy::cast_sign_loss
                        )]
                        let bps = (new.saturating_sub(old) as f64 * 8.0 / secs) as u64;
                        bps
                    };
                    out.insert(
                        link.name.clone(),
                        (rate(link.rx_bytes, *rx), rate(link.tx_bytes, *tx)),
                    );
                }
                next.insert(link.name.clone(), (now, link.rx_bytes, link.tx_bytes));
            }
            last = next;
            if let Ok(mut r) = rates.write() {
                *r = out;
            }
        }
    })
}
