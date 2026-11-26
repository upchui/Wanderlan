//! The traffic of the tunnels and trunk ports over the last hour, recorded
//! with every sample of the rates (every 2 s), for the graphs of the
//! terminal UI: they are full as soon as it starts.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use wanderlan_core::api::{RttPoint, TrafficPoint};
use wanderlan_core::identity::SiteId;

/// How far back the history goes.
pub const KEEP: Duration = Duration::from_secs(3600);

/// The history, shared between the sampler and the API.
pub type SharedHistory = Arc<RwLock<History>>;

/// The recorded traffic per interface.
#[derive(Debug, Default)]
pub struct History {
    /// The interfaces (tunnels and trunk ports) and peers to record, by
    /// network, set by its controller.
    wants: BTreeMap<String, (BTreeSet<String>, BTreeSet<SiteId>)>,
    /// The interfaces of all networks; the others are dropped.
    wanted: BTreeSet<String>,
    series: BTreeMap<String, VecDeque<TrafficPoint>>,
    /// Every probe per peer: its round trip, or lost.
    rtt: BTreeMap<SiteId, VecDeque<RttPoint>>,
}

impl History {
    /// Records only these interfaces and the probes of these peers of
    /// `network` from now on (and those of the other networks). A peer keeps
    /// its history when it is renamed: its identity and the name of its
    /// tunnel stay.
    pub fn want(&mut self, network: &str, names: BTreeSet<String>, peers: BTreeSet<SiteId>) {
        self.wants.insert(network.to_string(), (names, peers));
        let names: BTreeSet<String> = self.wants.values().flat_map(|(n, _)| n.clone()).collect();
        let peers: BTreeSet<SiteId> = self.wants.values().flat_map(|(_, p)| p.clone()).collect();
        self.series.retain(|name, _| names.contains(name));
        self.rtt.retain(|peer, _| peers.contains(peer));
        self.wanted = names;
    }

    /// Adds one sample of the rates (bit/s per interface) taken at `at_ms`;
    /// what is older than [`KEEP`] goes.
    pub fn record(&mut self, at_ms: u64, rates: &BTreeMap<String, (u64, u64)>) {
        // The clock went back: the points would be out of order (the graphs
        // could not find what is new); the history starts again.
        if self
            .series
            .values()
            .any(|p| p.back().is_some_and(|last| last.at() > at_ms))
        {
            self.series.clear();
            self.rtt.clear();
        }
        let oldest = at_ms.saturating_sub(duration_ms(KEEP));
        for name in &self.wanted {
            if let Some(&(rx, tx)) = rates.get(name) {
                self.series
                    .entry(name.clone())
                    .or_default()
                    .push_back(TrafficPoint(at_ms, rx, tx));
            }
        }
        for points in self.series.values_mut() {
            while points.front().is_some_and(|p| p.at() < oldest) {
                points.pop_front();
            }
        }
        self.series.retain(|_, points| !points.is_empty());
    }

    /// Adds one probe of `peer` at `at_ms`: its round trip in ms, or lost.
    pub fn record_rtt(&mut self, peer: SiteId, at_ms: u64, ms: Option<f32>) {
        let points = self.rtt.entry(peer).or_default();
        if points.back().is_some_and(|last| last.0 > at_ms) {
            points.clear();
        }
        points.push_back(RttPoint(at_ms, ms));
        let oldest = at_ms.saturating_sub(duration_ms(KEEP));
        while points.front().is_some_and(|p| p.0 < oldest) {
            points.pop_front();
        }
    }

    /// The probes of `peer` after `since_ms`, oldest first.
    pub fn rtt_since(&self, peer: SiteId, since_ms: u64) -> Vec<RttPoint> {
        self.rtt.get(&peer).map_or_else(Vec::new, |points| {
            let start = points.partition_point(|p| p.0 <= since_ms);
            points.range(start..).copied().collect()
        })
    }

    /// The points of `name` after `since_ms`, oldest first.
    pub fn since(&self, name: &str, since_ms: u64) -> Vec<TrafficPoint> {
        self.series.get(name).map_or_else(Vec::new, |points| {
            let start = points.partition_point(|p| p.at() <= since_ms);
            points.range(start..).copied().collect()
        })
    }
}

/// Now, in ms since the Unix epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, duration_ms)
}

/// A duration in ms.
pub fn duration_ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_hour_of_the_wanted_interfaces() {
        let a = SiteId::from_bytes([1; 32]);
        let mut h = History::default();
        h.want("default", ["vx-a".to_string()].into(), [a].into());
        // Another network does not take the interfaces of the first.
        h.want("lab", ["va-b".to_string()].into(), BTreeSet::new());
        let rates: BTreeMap<String, (u64, u64)> =
            [("vx-a".to_string(), (10, 20)), ("eth9".to_string(), (1, 1))].into();
        let hour = duration_ms(KEEP);
        h.record(1_000, &rates);
        h.record(3_000, &rates);
        assert_eq!(h.since("vx-a", 0).len(), 2);
        assert!(h.since("eth9", 0).is_empty(), "not wanted");
        assert_eq!(h.since("vx-a", 1_000), [TrafficPoint(3_000, 10, 20)]);
        // An hour later the first one is gone.
        h.record(1_000 + hour + 1, &rates);
        assert_eq!(
            h.since("vx-a", 0).first().map(TrafficPoint::at),
            Some(3_000)
        );
        h.record_rtt(a, 3_000, Some(4.0));
        assert_eq!(h.rtt_since(a, 0).len(), 1);
        // The clock went back: a new history.
        h.record(2_000, &[("vx-a".to_string(), (1, 1))].into());
        assert_eq!(h.since("vx-a", 0), [TrafficPoint(2_000, 1, 1)]);
        h.want("default", BTreeSet::new(), BTreeSet::new());
        assert!(h.since("vx-a", 0).is_empty());
        assert!(h.rtt_since(a, 0).is_empty());
    }
}
